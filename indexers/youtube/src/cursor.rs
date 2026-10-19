//! The opaque page cursor handed to the host between paged searches.
//!
//! A search page reads one Data API page per lane: the unfiltered search
//! (`*`), one allow-listed channel, or one channel's uploads playlist. The
//! cursor records which lanes still have a page and Google's `nextPageToken`
//! for each, so the next invocation reads exactly those pages and nothing
//! else.
//!
//! Format: `yt1.<mode>.<pages read>.<lane>~<token>[,<lane>~<token>…]`, with
//! `mode` `s` (search) or `u` (uploads). Every character is from the URL-safe
//! base64 alphabet or a separator, and the cursor never carries the key.

use crate::api::is_id_char;

pub const UNFILTERED_LANE: &str = "*";
const PREFIX: &str = "yt1";
const MAX_TOKEN_LEN: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Search,
    Uploads,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCursor {
    pub mode: Mode,
    /// Pages this search has already read, including the one that produced
    /// this cursor.
    pub pages_read: u32,
    /// `(lane, nextPageToken)` for every lane with another page.
    pub lanes: Vec<(String, String)>,
}

impl PageCursor {
    pub fn encode(&self) -> String {
        let mode = match self.mode {
            Mode::Search => 's',
            Mode::Uploads => 'u',
        };
        let lanes = self
            .lanes
            .iter()
            .map(|(lane, token)| format!("{lane}~{token}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("{PREFIX}.{mode}.{}.{lanes}", self.pages_read)
    }

    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.splitn(4, '.');
        if parts.next()? != PREFIX {
            return None;
        }
        let mode = match parts.next()? {
            "s" => Mode::Search,
            "u" => Mode::Uploads,
            _ => return None,
        };
        let pages_read = parts.next()?.parse::<u32>().ok()?;
        let mut lanes = Vec::new();
        for entry in parts.next()?.split(',') {
            let (lane, token) = entry.split_once('~')?;
            let lane_ok = lane == UNFILTERED_LANE
                || (!lane.is_empty() && lane.len() <= 64 && lane.chars().all(is_id_char));
            if !lane_ok || !is_page_token(token) {
                return None;
            }
            if lanes
                .iter()
                .any(|(existing, _): &(String, String)| existing == lane)
            {
                return None;
            }
            lanes.push((lane.to_string(), token.to_string()));
        }
        (!lanes.is_empty()).then_some(Self {
            mode,
            pages_read,
            lanes,
        })
    }
}

/// Google page tokens are short URL-safe base64 strings.
pub fn is_page_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_LEN
        && token.chars().all(|ch| is_id_char(ch) || ch == '=')
}
