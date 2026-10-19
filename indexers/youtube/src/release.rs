//! Turning a listed video and its `videos.list` enrichment into a Scryer
//! HTTP-family release.

use std::collections::HashMap;

use scryer_plugin_sdk::{
    HttpAvailability, HttpRangeSupport, HttpResourceRole, HttpTransport, IndexerProtocol,
    IndexerSourceKind, PluginHttpChannel, PluginHttpPlaylistPosition, PluginHttpRelease,
    PluginHttpResource, PluginHttpValidators, PluginSearchResult,
};
use serde_json::{Value, json};

use crate::api::Video;
use crate::text::{clean_title, parse_iso8601_duration};

pub const PROVIDER: &str = "youtube";

pub fn watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

pub fn guid(video_id: &str) -> String {
    format!("{PROVIDER}:{video_id}")
}

/// A video as a listing (`search.list` or `playlistItems.list`) describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Candidate {
    pub video_id: String,
    /// Already free of HTML entities.
    pub title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub published_at: Option<String>,
    pub live_broadcast_content: Option<String>,
    pub thumbnail_url: Option<String>,
    /// The listing itself marked the video private.
    pub listed_private: bool,
    pub playlist: Option<PluginHttpPlaylistPosition>,
}

/// What the `videos.list` enrichment said about one candidate.
#[derive(Clone, Copy, Debug)]
pub enum Enrichment<'a> {
    Found(&'a Video),
    /// Enrichment ran and Google did not return the video: it is deleted,
    /// or private to its owner.
    Missing,
    /// Enrichment could not run; nothing is known beyond the listing.
    NotFetched,
}

pub fn availability(
    candidate: &Candidate,
    enrichment: Enrichment<'_>,
    region: Option<&str>,
) -> HttpAvailability {
    let video = match enrichment {
        Enrichment::Found(video) => video,
        Enrichment::Missing if candidate.listed_private => return HttpAvailability::AuthRequired,
        Enrichment::Missing => return HttpAvailability::Removed,
        Enrichment::NotFetched => {
            return match candidate.live_broadcast_content.as_deref() {
                Some("live") => HttpAvailability::Live,
                Some("upcoming") => HttpAvailability::Upcoming,
                _ if candidate.listed_private => HttpAvailability::AuthRequired,
                _ => HttpAvailability::Unknown,
            };
        }
    };

    let status = video.status.as_ref();
    if status
        .and_then(|status| status.upload_status.as_deref())
        .is_some_and(|upload| matches!(upload, "deleted" | "rejected" | "failed"))
    {
        return HttpAvailability::Removed;
    }
    if let Some(region) = region
        && video
            .content_details
            .as_ref()
            .and_then(|details| details.region_restriction.as_ref())
            .is_some_and(|restriction| restriction.excludes(region))
    {
        return HttpAvailability::GeoRestricted;
    }
    let live = video
        .snippet
        .as_ref()
        .and_then(|snippet| snippet.live_broadcast_content.as_deref())
        .or(candidate.live_broadcast_content.as_deref());
    match live {
        Some("live") => return HttpAvailability::Live,
        Some("upcoming") => return HttpAvailability::Upcoming,
        _ => {}
    }
    if status.and_then(|status| status.privacy_status.as_deref()) == Some("private") {
        return HttpAvailability::AuthRequired;
    }
    HttpAvailability::Available
}

/// The provider page as the release's only resource: a client that declares
/// `provider_page` resolves the media itself.
pub fn provider_page_resource(page_url: &str) -> PluginHttpResource {
    PluginHttpResource {
        transport: HttpTransport::ProviderPage,
        role: HttpResourceRole::Muxed,
        url: page_url.to_string(),
        expires_at: None,
        headers: Vec::new(),
        auth_ref: None,
        size_bytes: None,
        size_estimate: None,
        validators: PluginHttpValidators::default(),
        container: None,
        video: None,
        audio: None,
        subtitle: None,
        separate_tracks: false,
        range_support: HttpRangeSupport::Unknown,
    }
}

pub fn build_result(
    candidate: &Candidate,
    enrichment: Enrichment<'_>,
    region: Option<&str>,
) -> PluginSearchResult {
    let video = match enrichment {
        Enrichment::Found(video) => Some(video),
        _ => None,
    };
    let snippet = video.and_then(|video| video.snippet.as_ref());
    let details = video.and_then(|video| video.content_details.as_ref());
    let status = video.and_then(|video| video.status.as_ref());

    let page_url = watch_url(&candidate.video_id);
    let channel_id = non_empty(snippet.and_then(|snippet| snippet.channel_id.clone()))
        .or_else(|| non_empty(candidate.channel_id.clone()));
    let channel_title = non_empty(snippet.and_then(|snippet| snippet.channel_title.clone()))
        .or_else(|| non_empty(candidate.channel_title.clone()));
    let raw_title = non_empty(snippet.and_then(|snippet| snippet.title.clone()))
        .or_else(|| non_empty(candidate.title.clone()))
        .unwrap_or_else(|| candidate.video_id.clone());
    let title = clean_title(&raw_title, channel_title.as_deref());
    let published_at = non_empty(snippet.and_then(|snippet| snippet.published_at.clone()))
        .or_else(|| non_empty(candidate.published_at.clone()));
    let duration_seconds = details
        .and_then(|details| details.duration.as_deref())
        .and_then(parse_iso8601_duration)
        .filter(|seconds| *seconds > 0);
    let languages = snippet
        .and_then(|snippet| snippet.default_audio_language.as_deref())
        .map(str::trim)
        .filter(|language| !language.is_empty() && !matches!(*language, "zxx" | "und"))
        .map(|language| vec![language.to_string()])
        .unwrap_or_default();
    let thumbnail_url = snippet
        .and_then(|snippet| snippet.thumbnails.as_ref())
        .and_then(|thumbnails| thumbnails.preferred_url())
        .or_else(|| candidate.thumbnail_url.clone());
    let availability = availability(candidate, enrichment, region);

    let mut extra: HashMap<String, Value> = HashMap::new();
    extra.insert("raw_title".into(), json!(raw_title));
    insert(&mut extra, "channel_id", channel_id.clone());
    insert(&mut extra, "channel_title", channel_title.clone());
    insert(
        &mut extra,
        "category_id",
        snippet.and_then(|snippet| non_empty(snippet.category_id.clone())),
    );
    if let Some(snippet) = snippet
        && !snippet.tags.is_empty()
    {
        extra.insert("tags".into(), json!(snippet.tags));
    }
    insert(
        &mut extra,
        "licensed_content",
        details.and_then(|details| details.licensed_content),
    );
    insert(
        &mut extra,
        "made_for_kids",
        status.and_then(|status| status.made_for_kids),
    );
    insert(
        &mut extra,
        "definition",
        details.and_then(|details| non_empty(details.definition.clone())),
    );
    insert(
        &mut extra,
        "caption",
        details
            .and_then(|details| details.caption.as_deref())
            .and_then(|caption| match caption {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }),
    );
    insert(
        &mut extra,
        "privacy_status",
        status.and_then(|status| non_empty(status.privacy_status.clone())),
    );
    insert(
        &mut extra,
        "live_broadcast_content",
        snippet
            .and_then(|snippet| non_empty(snippet.live_broadcast_content.clone()))
            .or_else(|| non_empty(candidate.live_broadcast_content.clone())),
    );
    insert(&mut extra, "thumbnail_url", thumbnail_url);
    insert(&mut extra, "duration_seconds", duration_seconds);
    extra.insert(
        "enriched".into(),
        json!(!matches!(enrichment, Enrichment::NotFetched)),
    );

    PluginSearchResult {
        title,
        link: Some(page_url.clone()),
        published_at,
        languages,
        provider_extra: extra,
        guid: Some(guid(&candidate.video_id)),
        info_url: Some(page_url.clone()),
        source_kind: Some(IndexerSourceKind::Http),
        protocol: Some(IndexerProtocol::Http),
        http: Some(PluginHttpRelease {
            provider: PROVIDER.to_string(),
            item_id: candidate.video_id.clone(),
            page_url: page_url.clone(),
            duration_seconds,
            channel: channel_title.map(|name| PluginHttpChannel {
                id: channel_id,
                name,
            }),
            availability,
            rights_window: None,
            playlist: candidate.playlist.clone(),
            resources: vec![provider_page_resource(&page_url)],
            resolve: None,
        }),
        ..PluginSearchResult::default()
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn insert<T: serde::Serialize>(extra: &mut HashMap<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
        && !value.is_null()
    {
        extra.insert(key.to_string(), value);
    }
}
