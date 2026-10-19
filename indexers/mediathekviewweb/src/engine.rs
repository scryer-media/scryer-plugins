//! One paged search invocation: the query to send, and how its answer
//! becomes one response page.
//!
//! The HTTP transport is a trait so the whole flow runs against fixtures in
//! native tests.

use scryer_plugin_sdk::{
    PluginSearchRequest as SearchRequest, PluginSearchResponse as SearchResponse,
};

use crate::api::{self, Entry, MAX_OFFSET, MAX_RESULTS, MvwConfig, MvwError, QueryCall};
use crate::cursor::PageCursor;
use crate::release::{build_result, variant};

/// Pages one search may read before it stops. The host keeps following the
/// cursor until it has as many results as it asked for; MediathekViewWeb is
/// a free community service, so the plugin bounds that here.
pub const PAGE_CEILING: u32 = 2;

pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// One `POST` of a query body to the API.
pub trait Fetch {
    async fn post(&self, call: &QueryCall) -> Result<HttpReply, MvwError>;
}

#[derive(Debug)]
pub struct PageOutcome {
    pub response: SearchResponse,
    /// The search stopped at [`PAGE_CEILING`] with more results unread.
    pub ceiling_reached: bool,
}

/// The text sent to MediathekViewWeb: the request's query with its
/// whitespace collapsed. Season and episode numbers mean nothing to the
/// film list, so they are not added.
pub fn build_query(request: &SearchRequest) -> String {
    request
        .query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn page_size(request: &SearchRequest) -> usize {
    if request.limit == 0 {
        MAX_RESULTS
    } else {
        request.limit.min(MAX_RESULTS)
    }
}

/// Whether the configuration keeps this entry.
pub fn keep(config: &MvwConfig, entry: &Entry) -> bool {
    if !config.allows_channel(&entry.channel) {
        return false;
    }
    let variant = variant(entry);
    if variant.audio_description && !config.include_audio_description {
        return false;
    }
    if variant.sign_language && !config.include_sign_language {
        return false;
    }
    if let (Some(minimum), Some(duration)) = (config.min_duration_seconds, entry.duration)
        && duration > 0
        && duration < minimum
    {
        return false;
    }
    true
}

pub async fn run_page<F: Fetch>(
    config: &MvwConfig,
    request: &SearchRequest,
    fetch: &F,
) -> Result<PageOutcome, MvwError> {
    let query = build_query(request);
    if query.is_empty() {
        // No query: a recent-releases or id-only request this indexer does
        // not serve. No call is made.
        return Ok(PageOutcome {
            response: SearchResponse::default(),
            ceiling_reached: false,
        });
    }

    let cursor = match request.page_cursor.as_deref() {
        None => None,
        Some(raw) => Some(PageCursor::decode(raw).ok_or(MvwError::InvalidCursor)?),
    };
    let pages_read = cursor.map_or(0, |cursor| cursor.pages_read);
    if pages_read >= PAGE_CEILING {
        // A cursor already at the ceiling reads nothing more.
        return Ok(PageOutcome {
            response: SearchResponse::default(),
            ceiling_reached: true,
        });
    }

    let size = page_size(request);
    let offset = cursor.map_or(0, |cursor| cursor.offset);
    let call = QueryCall {
        query,
        // One allowed channel narrows the query itself, so a page is not
        // spent on other channels; a longer list is filtered below.
        channel: match config.channels.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        },
        offset,
        size,
    };
    let reply = fetch.post(&call).await?;
    let result = api::parse_reply(reply.status, &reply.body)?;

    let read = result.results.len();
    let results = result
        .results
        .iter()
        .filter(|entry| keep(config, entry))
        .filter_map(build_result)
        .collect::<Vec<_>>();

    let next_offset = offset.saturating_add(read);
    let total = result
        .query_info
        .as_ref()
        .and_then(|info| info.total_results);
    let more = read >= size
        && next_offset <= MAX_OFFSET
        && total.is_none_or(|total| (next_offset as u64) < total);

    let pages_read = pages_read.saturating_add(1);
    let mut ceiling_reached = false;
    let next_cursor = if !more {
        None
    } else if pages_read >= PAGE_CEILING {
        ceiling_reached = true;
        None
    } else {
        Some(
            PageCursor {
                pages_read,
                offset: next_offset,
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
    })
}
