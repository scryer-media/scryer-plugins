//! One paged search invocation: which Data API pages to read, in what
//! order, and how their answers become one response page.
//!
//! The HTTP transport is a trait so the whole flow runs against fixtures in
//! native tests.

use std::collections::HashMap;

use scryer_plugin_sdk::{
    PluginHttpPlaylistPosition, PluginSearchRequest as SearchRequest,
    PluginSearchResponse as SearchResponse,
};

use crate::api::{
    self, ApiCall, ChannelListResponse, MAX_RESULTS, PlaylistItemListResponse, SearchListResponse,
    Video, VideoListResponse, YoutubeConfig, YoutubeError, is_video_id,
};
use crate::cursor::{Mode, PageCursor, UNFILTERED_LANE, is_page_token};
use crate::release::{Candidate, Enrichment, build_result};
use crate::text::{html_unescape, matches_query};

/// Uploads pages one search may read per channel before it stops. Each page
/// is 50 uploads and costs one unit per channel from the general bucket.
pub const UPLOADS_PAGE_CEILING: u32 = 4;

/// `search.list` pages one search may read per lane before it stops. The
/// host keeps following the cursor until it has as many results as it asked
/// for, and that bucket is 100 calls a day, so the plugin bounds it here.
pub const SEARCH_PAGE_CEILING: u32 = 2;

pub fn page_ceiling(mode: Mode) -> u32 {
    match mode {
        Mode::Search => SEARCH_PAGE_CEILING,
        Mode::Uploads => UPLOADS_PAGE_CEILING,
    }
}

pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// One GET against the Data API. Implementations attach the key as a header;
/// the call itself never carries it.
pub trait Fetch {
    async fn get(&self, call: &ApiCall) -> Result<HttpReply, YoutubeError>;
}

#[derive(Debug)]
pub struct PageOutcome {
    pub response: SearchResponse,
    /// The search stopped at its mode's page ceiling
    /// ([`SEARCH_PAGE_CEILING`] or [`UPLOADS_PAGE_CEILING`]) with more pages
    /// unread.
    pub ceiling_reached: bool,
    /// Diagnostics for the host log. Never contain the key.
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Failure {
    pub error: YoutubeError,
    /// Results read before the failure, without a cursor: the host replays
    /// the page.
    pub partial: Option<SearchResponse>,
    pub warnings: Vec<String>,
}

impl Failure {
    fn bare(error: YoutubeError) -> Self {
        Self {
            error,
            partial: None,
            warnings: Vec::new(),
        }
    }
}

/// The `q` sent to `search.list`: the title, plus the episode or season the
/// host asked for in the words uploaders use.
pub fn build_search_query(request: &SearchRequest) -> String {
    let base = request
        .query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match (request.season, request.episode) {
        (_, Some(episode)) => format!("{base} episode {episode}"),
        (Some(season), None) => format!("{base} season {season}"),
        (None, None) => base,
    }
}

pub fn page_size(request: &SearchRequest) -> usize {
    if request.limit == 0 {
        MAX_RESULTS
    } else {
        request.limit.min(MAX_RESULTS)
    }
}

/// Uploads enumeration replaces `search.list` when the operator has named
/// the channels and the host asks for a season or an episode.
pub fn choose_mode(config: &YoutubeConfig, request: &SearchRequest) -> Mode {
    if !config.channel_ids.is_empty() && (request.season.is_some() || request.episode.is_some()) {
        Mode::Uploads
    } else {
        Mode::Search
    }
}

struct Listed {
    candidates: Vec<Candidate>,
    next_lanes: Vec<(String, String)>,
    /// A lane failed after other lanes had produced candidates.
    pending_error: Option<YoutubeError>,
    warnings: Vec<String>,
}

pub async fn run_page<F: Fetch>(
    config: &YoutubeConfig,
    request: &SearchRequest,
    fetch: &F,
) -> Result<PageOutcome, Failure> {
    if request.query.trim().is_empty() {
        // No query: a recent-releases or id-only request this indexer does
        // not serve. No call is made.
        return Ok(PageOutcome {
            response: SearchResponse::default(),
            ceiling_reached: false,
            warnings: Vec::new(),
        });
    }

    let cursor = match request.page_cursor.as_deref() {
        None => None,
        Some(raw) => Some(
            PageCursor::decode(raw).ok_or_else(|| Failure::bare(YoutubeError::InvalidCursor))?,
        ),
    };
    let mode = cursor
        .as_ref()
        .map(|cursor| cursor.mode)
        .unwrap_or_else(|| choose_mode(config, request));
    let pages_read = cursor.as_ref().map_or(0, |cursor| cursor.pages_read);
    if pages_read >= page_ceiling(mode) {
        // A cursor already at the ceiling reads nothing more.
        return Ok(PageOutcome {
            response: SearchResponse::default(),
            ceiling_reached: true,
            warnings: Vec::new(),
        });
    }

    let listed = match mode {
        Mode::Search => list_search(config, request, cursor.as_ref(), fetch).await?,
        Mode::Uploads => list_uploads(config, request, cursor.as_ref(), fetch).await?,
    };
    let Listed {
        candidates,
        next_lanes,
        pending_error,
        mut warnings,
    } = listed;

    let videos = enrich(&candidates, fetch, &mut warnings).await;
    let region = config.region_code.as_deref();
    let results = candidates
        .iter()
        .map(|candidate| {
            let enrichment = match &videos {
                None => Enrichment::NotFetched,
                Some(videos) => videos
                    .get(&candidate.video_id)
                    .map_or(Enrichment::Missing, Enrichment::Found),
            };
            build_result(candidate, enrichment, region)
        })
        .collect::<Vec<_>>();

    if let Some(error) = pending_error {
        return Err(Failure {
            error,
            partial: Some(SearchResponse {
                results,
                ..SearchResponse::default()
            }),
            warnings,
        });
    }

    let pages_read = pages_read.saturating_add(1);
    let mut ceiling_reached = false;
    let next_cursor = if next_lanes.is_empty() {
        None
    } else if pages_read >= page_ceiling(mode) {
        ceiling_reached = true;
        None
    } else {
        Some(
            PageCursor {
                mode,
                pages_read,
                lanes: next_lanes,
            }
            .encode(),
        )
    };

    Ok(PageOutcome {
        response: SearchResponse {
            results,
            next_cursor,
            ..SearchResponse::default()
        },
        ceiling_reached,
        warnings,
    })
}

/// The lanes this page reads: `(lane, page token)`.
fn search_lanes(
    config: &YoutubeConfig,
    cursor: Option<&PageCursor>,
) -> Result<Vec<(String, Option<String>)>, YoutubeError> {
    match cursor {
        None if config.channel_ids.is_empty() => Ok(vec![(UNFILTERED_LANE.to_string(), None)]),
        None => Ok(config
            .channel_ids
            .iter()
            .map(|channel| (channel.clone(), None))
            .collect()),
        Some(cursor) => {
            // A cursor only continues lanes the current configuration
            // still searches.
            let allowed = |lane: &str| {
                if config.channel_ids.is_empty() {
                    lane == UNFILTERED_LANE
                } else {
                    config.channel_ids.iter().any(|channel| channel == lane)
                }
            };
            if cursor.lanes.iter().any(|(lane, _)| !allowed(lane)) {
                return Err(YoutubeError::InvalidCursor);
            }
            Ok(cursor
                .lanes
                .iter()
                .map(|(lane, token)| (lane.clone(), Some(token.clone())))
                .collect())
        }
    }
}

async fn list_search<F: Fetch>(
    config: &YoutubeConfig,
    request: &SearchRequest,
    cursor: Option<&PageCursor>,
    fetch: &F,
) -> Result<Listed, Failure> {
    let lanes = search_lanes(config, cursor).map_err(Failure::bare)?;
    let query = build_search_query(request);
    let size = page_size(request);
    let mut listed = Listed {
        candidates: Vec::new(),
        next_lanes: Vec::new(),
        pending_error: None,
        warnings: Vec::new(),
    };

    for (lane, token) in lanes {
        let channel = (lane != UNFILTERED_LANE).then_some(lane.as_str());
        let call = api::search_call(config, &query, size, channel, token.as_deref());
        let page: SearchListResponse = match get_json(fetch, &call).await {
            Ok(page) => page,
            Err(error) => {
                if listed.candidates.is_empty() {
                    return Err(Failure {
                        error,
                        partial: None,
                        warnings: listed.warnings,
                    });
                }
                listed.pending_error = Some(error);
                break;
            }
        };
        for item in page.items {
            if item
                .id
                .kind
                .as_deref()
                .is_some_and(|kind| kind != "youtube#video")
            {
                continue;
            }
            let Some(video_id) = item.id.video_id.filter(|id| is_video_id(id)) else {
                continue;
            };
            if listed
                .candidates
                .iter()
                .any(|candidate| candidate.video_id == video_id)
            {
                continue;
            }
            let snippet = item.snippet.unwrap_or_default();
            listed.candidates.push(Candidate {
                video_id,
                title: snippet.title.as_deref().map(html_unescape),
                channel_id: snippet.channel_id,
                channel_title: snippet.channel_title.as_deref().map(html_unescape),
                published_at: snippet.published_at,
                live_broadcast_content: snippet.live_broadcast_content,
                thumbnail_url: snippet
                    .thumbnails
                    .and_then(|thumbnails| thumbnails.preferred_url()),
                listed_private: false,
                playlist: None,
            });
        }
        if let Some(next) = page.next_page_token.filter(|token| is_page_token(token)) {
            listed.next_lanes.push((lane, next));
        }
    }
    Ok(listed)
}

async fn list_uploads<F: Fetch>(
    config: &YoutubeConfig,
    request: &SearchRequest,
    cursor: Option<&PageCursor>,
    fetch: &F,
) -> Result<Listed, Failure> {
    let mut listed = Listed {
        candidates: Vec::new(),
        next_lanes: Vec::new(),
        pending_error: None,
        warnings: Vec::new(),
    };

    let lanes: Vec<(String, Option<String>)> = match cursor {
        Some(cursor) => cursor
            .lanes
            .iter()
            .map(|(playlist, token)| (playlist.clone(), Some(token.clone())))
            .collect(),
        None => {
            let call = api::channels_call(&config.channel_ids);
            let channels: ChannelListResponse =
                get_json(fetch, &call).await.map_err(Failure::bare)?;
            let mut lanes = Vec::new();
            for channel_id in &config.channel_ids {
                match channels
                    .items
                    .iter()
                    .find(|channel| &channel.id == channel_id)
                    .and_then(|channel| channel.uploads_playlist())
                {
                    Some(playlist) => lanes.push((playlist.to_string(), None)),
                    None => listed.warnings.push(format!(
                        "allow-listed channel {channel_id} was not found; skipping it"
                    )),
                }
            }
            if lanes.is_empty() {
                return Err(Failure {
                    error: YoutubeError::NotFound {
                        reason: "channelNotFound".to_string(),
                    },
                    partial: None,
                    warnings: listed.warnings,
                });
            }
            lanes
        }
    };

    for (playlist_id, token) in lanes {
        let call = api::playlist_items_call(&playlist_id, token.as_deref());
        let page: PlaylistItemListResponse = match get_json(fetch, &call).await {
            Ok(page) => page,
            Err(error) => {
                if listed.candidates.is_empty() {
                    return Err(Failure {
                        error,
                        partial: None,
                        warnings: listed.warnings,
                    });
                }
                listed.pending_error = Some(error);
                break;
            }
        };
        let total = page.page_info.as_ref().and_then(|info| info.total_results);
        for item in page.items {
            let snippet = item.snippet.unwrap_or_default();
            let Some(video_id) = item
                .content_details
                .as_ref()
                .and_then(|details| details.video_id.clone())
                .or_else(|| {
                    snippet
                        .resource_id
                        .as_ref()
                        .and_then(|resource| resource.video_id.clone())
                })
                .filter(|id| is_video_id(id))
            else {
                continue;
            };
            let title = snippet.title.clone().unwrap_or_default();
            let channel_title = snippet
                .video_owner_channel_title
                .clone()
                .or_else(|| snippet.channel_title.clone());
            if !matches_query(&request.query, &title, channel_title.as_deref()) {
                continue;
            }
            if listed
                .candidates
                .iter()
                .any(|candidate| candidate.video_id == video_id)
            {
                continue;
            }
            listed.candidates.push(Candidate {
                video_id,
                title: Some(title),
                channel_id: snippet
                    .video_owner_channel_id
                    .clone()
                    .or_else(|| snippet.channel_id.clone()),
                channel_title,
                published_at: item
                    .content_details
                    .as_ref()
                    .and_then(|details| details.video_published_at.clone())
                    .or_else(|| snippet.published_at.clone()),
                live_broadcast_content: None,
                thumbnail_url: snippet
                    .thumbnails
                    .as_ref()
                    .and_then(|thumbnails| thumbnails.preferred_url()),
                listed_private: item
                    .status
                    .as_ref()
                    .and_then(|status| status.privacy_status.as_deref())
                    == Some("private"),
                playlist: Some(PluginHttpPlaylistPosition {
                    id: playlist_id.clone(),
                    index: snippet.position,
                    count: total,
                }),
            });
        }
        if let Some(next) = page.next_page_token.filter(|token| is_page_token(token)) {
            listed.next_lanes.push((playlist_id, next));
        }
    }
    Ok(listed)
}

/// `videos.list` for every candidate, 50 ids per call. `None` when
/// enrichment could not run; the listing alone is then reported.
async fn enrich<F: Fetch>(
    candidates: &[Candidate],
    fetch: &F,
    warnings: &mut Vec<String>,
) -> Option<HashMap<String, Video>> {
    let mut videos = HashMap::new();
    let ids = candidates
        .iter()
        .map(|candidate| candidate.video_id.clone())
        .collect::<Vec<_>>();
    for chunk in ids.chunks(MAX_RESULTS) {
        let call = api::videos_call(chunk);
        match get_json::<_, VideoListResponse>(fetch, &call).await {
            Ok(page) => {
                for video in page.items {
                    if chunk.contains(&video.id) {
                        videos.insert(video.id.clone(), video);
                    }
                }
            }
            Err(error) => {
                warnings.push(format!(
                    "video details unavailable, reporting listing data only: {error}"
                ));
                return None;
            }
        }
    }
    Some(videos)
}

async fn get_json<F: Fetch, T: serde::de::DeserializeOwned>(
    fetch: &F,
    call: &ApiCall,
) -> Result<T, YoutubeError> {
    let reply = fetch.get(call).await?;
    if !(200..300).contains(&reply.status) {
        return Err(api::classify_error(reply.status, &reply.body));
    }
    serde_json::from_slice(&reply.body)
        .map_err(|error| YoutubeError::Malformed(format!("{} response: {error}", call.endpoint)))
}
