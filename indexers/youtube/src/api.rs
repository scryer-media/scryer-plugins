//! The YouTube Data API v3 surface this indexer uses: configuration, request
//! construction, response shapes and error classification. Everything here is
//! pure so it can be tested against fixture JSON without a host.

use std::fmt;

use serde::Deserialize;

pub const API_BASE: &str = "https://www.googleapis.com/youtube/v3";
pub const API_HOST: &str = "www.googleapis.com";
/// The largest `maxResults` the Data API accepts on a list call, and the
/// most ids one `videos.list` or `channels.list` call may name.
pub const MAX_RESULTS: usize = 50;
/// The most channels the allow-list may hold. Each channel costs one
/// `search.list` call per search page, and that bucket is small.
pub const MAX_CHANNELS: usize = 10;
/// The header that carries the API key, so the key never appears in a URL.
pub const API_KEY_HEADER: &str = "X-Goog-Api-Key";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafeSearch {
    None,
    Moderate,
    Strict,
}

impl SafeSearch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Moderate => "moderate",
            Self::Strict => "strict",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "moderate" => Some(Self::Moderate),
            "strict" => Some(Self::Strict),
            _ => None,
        }
    }
}

/// One configured indexer instance.
#[derive(Clone, PartialEq, Eq)]
pub struct YoutubeConfig {
    pub api_key: String,
    pub region_code: Option<String>,
    pub relevance_language: Option<String>,
    pub safe_search: SafeSearch,
    pub channel_ids: Vec<String>,
}

// Hand-written so the key can never reach a log line through `{:?}`.
impl fmt::Debug for YoutubeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YoutubeConfig")
            .field("api_key", &"[redacted]")
            .field("region_code", &self.region_code)
            .field("relevance_language", &self.relevance_language)
            .field("safe_search", &self.safe_search)
            .field("channel_ids", &self.channel_ids)
            .finish()
    }
}

impl YoutubeConfig {
    /// Build and validate a configuration from raw config values. Messages
    /// name the offending field and never echo the key.
    pub fn from_values(get: impl Fn(&str) -> Option<String>) -> Result<Self, YoutubeError> {
        let value = |key: &str| {
            get(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let api_key = value("api_key").ok_or_else(|| {
            YoutubeError::InvalidConfig("api_key is required: add a YouTube Data API key".into())
        })?;
        if api_key
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(YoutubeError::InvalidConfig(
                "api_key contains whitespace or control characters".into(),
            ));
        }

        let region_code = match value("region_code") {
            None => None,
            Some(region)
                if region.len() == 2 && region.chars().all(|ch| ch.is_ascii_alphabetic()) =>
            {
                Some(region.to_ascii_uppercase())
            }
            Some(_) => {
                return Err(YoutubeError::InvalidConfig(
                    "region_code must be a two-letter ISO 3166-1 alpha-2 code, for example US"
                        .into(),
                ));
            }
        };

        let relevance_language = match value("relevance_language") {
            None => None,
            Some(language) if is_language_tag(&language) => Some(language),
            Some(_) => {
                return Err(YoutubeError::InvalidConfig(
                    "relevance_language must be an ISO 639-1 code, for example en or zh-Hans"
                        .into(),
                ));
            }
        };

        let safe_search = match value("safe_search") {
            None => SafeSearch::Moderate,
            Some(raw) => SafeSearch::parse(&raw).ok_or_else(|| {
                YoutubeError::InvalidConfig(
                    "safe_search must be one of none, moderate or strict".into(),
                )
            })?,
        };

        let channel_ids = parse_channel_ids(value("channel_ids").as_deref().unwrap_or(""))?;

        Ok(Self {
            api_key,
            region_code,
            relevance_language,
            safe_search,
            channel_ids,
        })
    }
}

fn is_language_tag(value: &str) -> bool {
    let mut parts = value.split('-');
    let primary = parts.next().unwrap_or_default();
    if !(2..=3).contains(&primary.len()) || !primary.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }
    parts.all(|part| {
        (2..=8).contains(&part.len()) && part.chars().all(|ch| ch.is_ascii_alphanumeric())
    })
}

/// Channel ids from a tag list (comma, semicolon or newline separated, or a
/// JSON array), deduplicated in order.
pub fn parse_channel_ids(raw: &str) -> Result<Vec<String>, YoutubeError> {
    let raw = raw.trim();
    let parts: Vec<String> = if raw.starts_with('[') {
        serde_json::from_str::<Vec<String>>(raw).map_err(|_| {
            YoutubeError::InvalidConfig("channel_ids is not a valid list of channel ids".into())
        })?
    } else {
        raw.split([',', ';', '\n']).map(str::to_string).collect()
    };
    let mut ids: Vec<String> = Vec::new();
    for part in parts {
        let id = part.trim();
        if id.is_empty() {
            continue;
        }
        if !is_channel_id(id) {
            return Err(YoutubeError::InvalidConfig(format!(
                "channel_ids entry '{}' is not a YouTube channel id (UC followed by 22 characters)",
                sanitize_for_message(id)
            )));
        }
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    if ids.len() > MAX_CHANNELS {
        return Err(YoutubeError::InvalidConfig(format!(
            "channel_ids holds {} channels; at most {MAX_CHANNELS} are allowed because each one costs a search call",
            ids.len()
        )));
    }
    Ok(ids)
}

pub fn is_channel_id(value: &str) -> bool {
    value.len() == 24 && value.starts_with("UC") && value.chars().all(is_id_char)
}

pub fn is_video_id(value: &str) -> bool {
    (6..=32).contains(&value.len()) && value.chars().all(is_id_char)
}

pub fn is_id_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

fn sanitize_for_message(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(48)
        .collect()
}

/// One Data API call: an endpoint and its query parameters. The key is not a
/// parameter; it travels in [`API_KEY_HEADER`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiCall {
    pub endpoint: &'static str,
    pub params: Vec<(&'static str, String)>,
}

impl ApiCall {
    pub fn url(&self) -> String {
        let query = self
            .params
            .iter()
            .map(|(key, value)| format!("{key}={}", url_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{API_BASE}/{}?{query}", self.endpoint)
    }

    #[cfg(test)]
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }
}

/// `search.list` for one page of videos, optionally scoped to one channel.
pub fn search_call(
    config: &YoutubeConfig,
    query: &str,
    max_results: usize,
    channel_id: Option<&str>,
    page_token: Option<&str>,
) -> ApiCall {
    let mut params = vec![
        ("part", "snippet".to_string()),
        ("type", "video".to_string()),
        ("q", query.to_string()),
        ("maxResults", max_results.clamp(1, MAX_RESULTS).to_string()),
    ];
    if let Some(channel_id) = channel_id {
        params.push(("channelId", channel_id.to_string()));
    }
    if let Some(page_token) = page_token {
        params.push(("pageToken", page_token.to_string()));
    }
    if let Some(region) = &config.region_code {
        params.push(("regionCode", region.clone()));
    }
    if let Some(language) = &config.relevance_language {
        params.push(("relevanceLanguage", language.clone()));
    }
    params.push(("safeSearch", config.safe_search.as_str().to_string()));
    ApiCall {
        endpoint: "search",
        params,
    }
}

/// `videos.list` enrichment for up to [`MAX_RESULTS`] ids.
pub fn videos_call(ids: &[String]) -> ApiCall {
    ApiCall {
        endpoint: "videos",
        params: vec![
            ("part", "snippet,contentDetails,status".to_string()),
            ("id", ids.join(",")),
        ],
    }
}

/// `channels.list` for the uploads playlist of each allow-listed channel.
pub fn channels_call(ids: &[String]) -> ApiCall {
    ApiCall {
        endpoint: "channels",
        params: vec![
            ("part", "contentDetails".to_string()),
            ("id", ids.join(",")),
        ],
    }
}

/// One page of a channel's uploads playlist.
pub fn playlist_items_call(playlist_id: &str, page_token: Option<&str>) -> ApiCall {
    let mut params = vec![
        ("part", "snippet,contentDetails,status".to_string()),
        ("playlistId", playlist_id.to_string()),
        ("maxResults", MAX_RESULTS.to_string()),
    ];
    if let Some(page_token) = page_token {
        params.push(("pageToken", page_token.to_string()));
    }
    ApiCall {
        endpoint: "playlistItems",
        params,
    }
}

pub fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char)
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchListResponse {
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub items: Vec<SearchItem>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchItem {
    #[serde(default)]
    pub id: SearchItemId,
    #[serde(default)]
    pub snippet: Option<SearchSnippet>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchItemId {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub video_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSnippet {
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub thumbnails: Option<Thumbnails>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub live_broadcast_content: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Thumbnails {
    #[serde(default)]
    pub default: Option<Thumbnail>,
    #[serde(default)]
    pub medium: Option<Thumbnail>,
    #[serde(default)]
    pub high: Option<Thumbnail>,
    #[serde(default)]
    pub standard: Option<Thumbnail>,
    #[serde(default)]
    pub maxres: Option<Thumbnail>,
}

impl Thumbnails {
    /// The medium thumbnail, which is what a result list shows, falling back
    /// to whatever size is present.
    pub fn preferred_url(&self) -> Option<String> {
        [
            &self.medium,
            &self.high,
            &self.default,
            &self.standard,
            &self.maxres,
        ]
        .into_iter()
        .flatten()
        .map(|thumbnail| thumbnail.url.trim())
        .find(|url| url.starts_with("https://"))
        .map(str::to_string)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Thumbnail {
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct VideoListResponse {
    #[serde(default)]
    pub items: Vec<Video>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Video {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub snippet: Option<VideoSnippet>,
    #[serde(default)]
    pub content_details: Option<VideoContentDetails>,
    #[serde(default)]
    pub status: Option<VideoStatus>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSnippet {
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub thumbnails: Option<Thumbnails>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub category_id: Option<String>,
    #[serde(default)]
    pub live_broadcast_content: Option<String>,
    #[serde(default)]
    pub default_audio_language: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoContentDetails {
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub definition: Option<String>,
    /// `"true"` or `"false"`, as a string.
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub licensed_content: Option<bool>,
    #[serde(default)]
    pub region_restriction: Option<RegionRestriction>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RegionRestriction {
    #[serde(default)]
    pub allowed: Option<Vec<String>>,
    #[serde(default)]
    pub blocked: Option<Vec<String>>,
}

impl RegionRestriction {
    /// Whether `region` may not watch the video.
    pub fn excludes(&self, region: &str) -> bool {
        let listed = |list: &Option<Vec<String>>| {
            list.as_ref()
                .is_some_and(|codes| codes.iter().any(|code| code.eq_ignore_ascii_case(region)))
        };
        if let Some(allowed) = &self.allowed
            && !allowed.is_empty()
            && !listed(&self.allowed)
        {
            return true;
        }
        listed(&self.blocked)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStatus {
    #[serde(default)]
    pub upload_status: Option<String>,
    #[serde(default)]
    pub privacy_status: Option<String>,
    #[serde(default)]
    pub made_for_kids: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChannelListResponse {
    #[serde(default)]
    pub items: Vec<Channel>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub content_details: Option<ChannelContentDetails>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelContentDetails {
    #[serde(default)]
    pub related_playlists: Option<RelatedPlaylists>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RelatedPlaylists {
    #[serde(default)]
    pub uploads: Option<String>,
}

impl Channel {
    pub fn uploads_playlist(&self) -> Option<&str> {
        self.content_details
            .as_ref()?
            .related_playlists
            .as_ref()?
            .uploads
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.chars().all(is_id_char))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItemListResponse {
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub page_info: Option<PageInfo>,
    #[serde(default)]
    pub items: Vec<PlaylistItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    #[serde(default)]
    pub total_results: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItem {
    #[serde(default)]
    pub snippet: Option<PlaylistItemSnippet>,
    #[serde(default)]
    pub content_details: Option<PlaylistItemContentDetails>,
    #[serde(default)]
    pub status: Option<PlaylistItemStatus>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItemSnippet {
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub thumbnails: Option<Thumbnails>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub position: Option<u32>,
    #[serde(default)]
    pub resource_id: Option<ResourceId>,
    #[serde(default)]
    pub video_owner_channel_id: Option<String>,
    #[serde(default)]
    pub video_owner_channel_title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceId {
    #[serde(default)]
    pub video_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItemContentDetails {
    #[serde(default)]
    pub video_id: Option<String>,
    #[serde(default)]
    pub video_published_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItemStatus {
    #[serde(default)]
    pub privacy_status: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every failure the indexer reports. None of them carries the key, a URL or
/// an upstream body; reasons are Google's fixed reason codes, sanitised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum YoutubeError {
    /// The key is invalid, expired, or not permitted to call this API
    /// (400 `keyInvalid`, 403 `forbidden`, key restrictions).
    InvalidKey,
    /// The YouTube Data API v3 is not enabled for the key's project
    /// (403 `accessNotConfigured`).
    ApiDisabled,
    /// The project's daily quota is spent (403 `quotaExceeded`).
    QuotaExceeded,
    /// A short-term rate limit, as opposed to the daily quota.
    RateLimited,
    /// The channel, playlist or video does not exist (404).
    NotFound {
        reason: String,
    },
    /// Google refused a parameter (400), for example an invalid channel id.
    BadRequest {
        reason: String,
    },
    Upstream {
        status: u16,
    },
    Transport(String),
    Cancelled,
    Malformed(String),
    InvalidConfig(String),
    InvalidCursor,
}

impl fmt::Display for YoutubeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey => formatter.write_str(
                "YouTube rejected the configured API key: it is invalid, expired, or restricted from the YouTube Data API v3",
            ),
            Self::ApiDisabled => formatter.write_str(
                "the YouTube Data API v3 is not enabled for the API key's Google Cloud project",
            ),
            Self::QuotaExceeded => formatter.write_str(
                "the YouTube Data API daily quota for this API key's project is exhausted; searches resume when Google resets it",
            ),
            Self::RateLimited => formatter.write_str("YouTube rate limited the request; try again shortly"),
            Self::NotFound { reason } => {
                write!(formatter, "YouTube could not find the requested channel, playlist or video ({reason})")
            }
            Self::BadRequest { reason } => {
                write!(formatter, "YouTube refused the search parameters ({reason})")
            }
            Self::Upstream { status } => write!(formatter, "YouTube Data API returned HTTP {status}"),
            Self::Transport(detail) => write!(formatter, "YouTube Data API request failed: {detail}"),
            Self::Cancelled => formatter.write_str("YouTube search was cancelled"),
            Self::Malformed(detail) => write!(formatter, "YouTube Data API returned an unreadable response: {detail}"),
            Self::InvalidConfig(detail) => formatter.write_str(detail),
            Self::InvalidCursor => formatter.write_str("the page cursor is not one this indexer issued"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    errors: Vec<ErrorItem>,
    #[serde(default)]
    details: Vec<ErrorDetail>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorItem {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    reason: Option<String>,
}

const QUOTA_REASONS: &[&str] = &[
    "quotaExceeded",
    "dailyLimitExceeded",
    "dailyLimitExceededUnreg",
];
const RATE_REASONS: &[&str] = &[
    "rateLimitExceeded",
    "userRateLimitExceeded",
    "RATE_LIMIT_EXCEEDED",
];
const DISABLED_REASONS: &[&str] = &["accessNotConfigured", "SERVICE_DISABLED"];
const KEY_REASONS: &[&str] = &[
    "keyInvalid",
    "keyExpired",
    "ipRefererBlocked",
    "forbidden",
    "API_KEY_INVALID",
    "API_KEY_EXPIRED",
    "API_KEY_SERVICE_BLOCKED",
    "API_KEY_HTTP_REFERRER_BLOCKED",
    "API_KEY_IP_ADDRESS_BLOCKED",
    "API_KEY_ANDROID_APP_BLOCKED",
    "API_KEY_IOS_APP_BLOCKED",
];

/// Classify a non-2xx Data API response.
pub fn classify_error(status: u16, body: &[u8]) -> YoutubeError {
    let envelope = serde_json::from_slice::<ErrorEnvelope>(body).unwrap_or_default();
    let error = envelope.error.unwrap_or_default();
    let reasons: Vec<String> = error
        .errors
        .iter()
        .filter_map(|item| item.reason.clone())
        .chain(
            error
                .details
                .iter()
                .filter_map(|detail| detail.reason.clone()),
        )
        .collect();
    let has = |set: &[&str]| reasons.iter().any(|reason| set.contains(&reason.as_str()));

    if has(QUOTA_REASONS) {
        return YoutubeError::QuotaExceeded;
    }
    if has(RATE_REASONS) || status == 429 {
        return YoutubeError::RateLimited;
    }
    if has(DISABLED_REASONS) {
        return YoutubeError::ApiDisabled;
    }
    let message_names_key = error
        .message
        .as_deref()
        .is_some_and(|message| message.contains("API key"));
    if has(KEY_REASONS) || status == 401 || (status == 400 && message_names_key) {
        return YoutubeError::InvalidKey;
    }
    let first_reason = reasons
        .first()
        .map(|reason| sanitize_reason(reason))
        .filter(|reason| !reason.is_empty());
    if status == 404 || reasons.iter().any(|reason| reason.ends_with("NotFound")) {
        return YoutubeError::NotFound {
            reason: first_reason.unwrap_or_else(|| "notFound".to_string()),
        };
    }
    if status == 400 {
        return YoutubeError::BadRequest {
            reason: first_reason.unwrap_or_else(|| "badRequest".to_string()),
        };
    }
    YoutubeError::Upstream { status }
}

fn sanitize_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .take(64)
        .collect()
}
