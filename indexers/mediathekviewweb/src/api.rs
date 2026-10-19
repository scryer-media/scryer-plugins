//! The MediathekViewWeb query API this indexer uses: configuration, the
//! request body, response shapes and error classification. Everything here is
//! pure so it can be tested against fixture JSON without a host.

use std::fmt;

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

pub const API_URL: &str = "https://mediathekviewweb.de/api/query";
pub const API_HOST: &str = "mediathekviewweb.de";
/// The most results one query asks for. The API accepts more, but a page
/// this size already spans weeks of a weekly programme.
pub const MAX_RESULTS: usize = 50;
/// The most channels the allow-list may hold.
pub const MAX_CHANNELS: usize = 20;
/// The furthest offset a cursor may name: [`MAX_RESULTS`] times a generous
/// page count. Anything beyond it is not a cursor this plugin wrote.
pub const MAX_OFFSET: usize = 10_000;

/// One configured indexer instance. Nothing in it is secret.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MvwConfig {
    /// Broadcaster names as MediathekViewWeb spells them (`ARD`, `ZDF`,
    /// `KiKA`, …). Empty means every channel.
    pub channels: Vec<String>,
    pub include_audio_description: bool,
    pub include_sign_language: bool,
    /// Results shorter than this are dropped. `None` keeps every length.
    pub min_duration_seconds: Option<u64>,
}

impl MvwConfig {
    /// Build and validate a configuration from raw config values. Messages
    /// name the offending field.
    pub fn from_values(get: impl Fn(&str) -> Option<String>) -> Result<Self, MvwError> {
        let value = |key: &str| {
            get(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let channels = parse_channels(value("channels").as_deref().unwrap_or(""))?;
        let include_audio_description = parse_bool(
            "include_audio_description",
            value("include_audio_description"),
        )?;
        let include_sign_language =
            parse_bool("include_sign_language", value("include_sign_language"))?;
        let min_duration_seconds = match value("min_duration_minutes") {
            None => None,
            Some(raw) => {
                let minutes = raw
                    .parse::<f64>()
                    .ok()
                    .filter(|minutes| minutes.is_finite() && *minutes >= 0.0 && *minutes <= 1440.0)
                    .ok_or_else(|| {
                        MvwError::InvalidConfig(
                            "min_duration_minutes must be a number of minutes between 0 and 1440"
                                .into(),
                        )
                    })?;
                let seconds = (minutes * 60.0).round() as u64;
                (seconds > 0).then_some(seconds)
            }
        };

        Ok(Self {
            channels,
            include_audio_description,
            include_sign_language,
            min_duration_seconds,
        })
    }

    /// Whether a result from `channel` passes the allow-list.
    pub fn allows_channel(&self, channel: &str) -> bool {
        self.channels.is_empty()
            || self
                .channels
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(channel.trim()))
    }
}

fn parse_bool(key: &str, raw: Option<String>) -> Result<bool, MvwError> {
    match raw.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => Ok(false),
        Some("true" | "1" | "yes" | "on") => Ok(true),
        Some("false" | "0" | "no" | "off") => Ok(false),
        Some(_) => Err(MvwError::InvalidConfig(format!(
            "{key} must be true or false"
        ))),
    }
}

/// Channel names from a tag list (comma, semicolon or newline separated, or a
/// JSON array), deduplicated case-insensitively in order.
pub fn parse_channels(raw: &str) -> Result<Vec<String>, MvwError> {
    let raw = raw.trim();
    let parts: Vec<String> = if raw.starts_with('[') {
        serde_json::from_str::<Vec<String>>(raw).map_err(|_| {
            MvwError::InvalidConfig("channels is not a valid list of channel names".into())
        })?
    } else {
        raw.split([',', ';', '\n']).map(str::to_string).collect()
    };
    let mut channels: Vec<String> = Vec::new();
    for part in parts {
        let channel = part.trim();
        if channel.is_empty() {
            continue;
        }
        if channel.len() > 64 || channel.chars().any(char::is_control) {
            return Err(MvwError::InvalidConfig(
                "channels entries must be channel names such as ARD, ZDF or KiKA".into(),
            ));
        }
        if !channels
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(channel))
        {
            channels.push(channel.to_string());
        }
    }
    if channels.len() > MAX_CHANNELS {
        return Err(MvwError::InvalidConfig(format!(
            "channels holds {} entries; at most {MAX_CHANNELS} are allowed",
            channels.len()
        )));
    }
    Ok(channels)
}

/// One `POST /api/query`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCall {
    /// Matched against title and topic.
    pub query: String,
    /// Matched against the channel field. Set only for a one-entry
    /// allow-list; a longer list is filtered after the call.
    pub channel: Option<String>,
    pub offset: usize,
    pub size: usize,
}

impl QueryCall {
    /// The JSON document the API reads. It is sent as `text/plain`, which is
    /// what the MediathekViewWeb web client sends.
    pub fn body(&self) -> String {
        let mut queries = vec![json!({"fields": ["title", "topic"], "query": self.query})];
        if let Some(channel) = &self.channel {
            queries.push(json!({"fields": ["channel"], "query": channel}));
        }
        json!({
            "queries": queries,
            "sortBy": "timestamp",
            "sortOrder": "desc",
            "future": false,
            "offset": self.offset,
            "size": self.size.clamp(1, MAX_RESULTS),
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct QueryResponse {
    #[serde(default)]
    pub result: Option<QueryResult>,
    #[serde(default)]
    pub err: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    #[serde(default)]
    pub results: Vec<Entry>,
    #[serde(default)]
    pub query_info: Option<QueryInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryInfo {
    #[serde(default, deserialize_with = "lenient_u64")]
    pub total_results: Option<u64>,
}

/// One film-list entry.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Entry {
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub channel: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub topic: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub title: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub description: String,
    /// Unix seconds.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub timestamp: Option<u64>,
    /// Seconds.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub duration: Option<u64>,
    /// Bytes, a rounded estimate for `url_video`.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub size: Option<u64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub url_website: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub url_subtitle: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub url_video: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub url_video_low: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub url_video_hd: String,
    #[serde(
        default,
        rename = "filmlisteTimestamp",
        deserialize_with = "lenient_u64"
    )]
    pub filmliste_timestamp: Option<u64>,
}

/// A number, a numeric string, or nothing. The film list is assembled from
/// many broadcaster crawlers and its numeric fields are not always numbers.
fn lenient_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| *value >= 0.0)
                .map(|value| value as u64)
        }),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    })
}

/// A string, or an empty string for anything else.
fn lenient_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    })
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MvwError {
    InvalidConfig(String),
    InvalidCursor,
    /// HTTP 429.
    RateLimited,
    /// HTTP 5xx, with the API's own message when it sent one.
    Upstream {
        status: u16,
        message: Option<String>,
    },
    /// Any other non-success status.
    BadStatus {
        status: u16,
        message: Option<String>,
    },
    /// The API answered and named an error in `err`.
    Rejected(String),
    /// The body was not the documented JSON.
    Malformed(String),
    Transport(String),
    Cancelled,
}

impl fmt::Display for MvwError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let suffix = |message: &Option<String>| {
            message
                .as_deref()
                .map(|message| format!(": {message}"))
                .unwrap_or_default()
        };
        match self {
            Self::InvalidConfig(message) => write!(formatter, "invalid configuration: {message}"),
            Self::InvalidCursor => write!(
                formatter,
                "the page cursor was not written by this indexer or no longer fits its configuration"
            ),
            Self::RateLimited => write!(formatter, "MediathekViewWeb is rate limiting requests"),
            Self::Upstream { status, message } => write!(
                formatter,
                "MediathekViewWeb is unavailable (HTTP {status}){}",
                suffix(message)
            ),
            Self::BadStatus { status, message } => write!(
                formatter,
                "MediathekViewWeb refused the query (HTTP {status}){}",
                suffix(message)
            ),
            Self::Rejected(message) => {
                write!(formatter, "MediathekViewWeb reported an error: {message}")
            }
            Self::Malformed(message) => {
                write!(
                    formatter,
                    "MediathekViewWeb sent an unreadable response: {message}"
                )
            }
            Self::Transport(message) => {
                write!(
                    formatter,
                    "MediathekViewWeb could not be reached: {message}"
                )
            }
            Self::Cancelled => write!(formatter, "the search was cancelled"),
        }
    }
}

/// The text of an `err` value: a string, a list of strings, or anything
/// else rendered as JSON. `None` when there is no error.
pub fn error_text(err: &Value) -> Option<String> {
    let text = match err {
        Value::Null => return None,
        Value::String(text) => text.trim().to_string(),
        Value::Array(items) if items.is_empty() => return None,
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.trim().to_string(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    };
    Some(sanitize(&text))
}

fn sanitize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "unspecified error".to_string()
    } else {
        cleaned
    }
}

/// The API's message from an error body, when it is the documented JSON.
fn body_message(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<QueryResponse>(body)
        .ok()
        .and_then(|response| response.err)
        .and_then(|err| error_text(&err))
}

/// Read one reply: status first, then the body.
pub fn parse_reply(status: u16, body: &[u8]) -> Result<QueryResult, MvwError> {
    match status {
        200..=299 => {}
        429 => return Err(MvwError::RateLimited),
        500..=599 => {
            return Err(MvwError::Upstream {
                status,
                message: body_message(body),
            });
        }
        _ => {
            return Err(MvwError::BadStatus {
                status,
                message: body_message(body),
            });
        }
    }
    let response: QueryResponse = serde_json::from_slice(body)
        .map_err(|error| MvwError::Malformed(format!("query response: {error}")))?;
    if let Some(message) = response.err.as_ref().and_then(error_text) {
        return Err(MvwError::Rejected(message));
    }
    response
        .result
        .ok_or_else(|| MvwError::Malformed("query response has no result".into()))
}
