//! Pushover push notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Pushover notification (`src/NzbDrone.Core/Notifications/Pushover/`)
//! is a thin form POST: a fixed per-event title constant, the event's prose
//! message, and six settings copied straight onto form parameters
//! (`PushoverProxy.cs:36-80`). Everything it can get wrong it gets wrong
//! silently — `_httpClient.Post(request)` throws on a 4xx and the exception is
//! only ever caught inside `Test`, where every failure is blamed on `ApiKey`
//! (`PushoverProxy.cs:82-98`). Its settings validator has a copy-paste bug: the
//! `Expire` rule was written against `Retry` twice (`PushoverSettings.cs:14-15`),
//! so `Expire` is never validated at all.
//!
//! The June port copied that shape and then reported its *own* configuration
//! checks as delivery failures, which tells the operator "a notification failed
//! to send" when the truth is "priority 2 needs a retry interval".
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * every configuration problem is a typed `PluginError` naming the field —
//!   `priority`, `retry`, `expire`, `ttl`, `devices`, `encryption_key`,
//!   `metadata_link`, `api_key`, `user_key` — instead of a fake delivery
//!   failure;
//! * Pushover's own error JSON (`{"user":"invalid","errors":[…],"status":0}`) is
//!   parsed and attributed to the offending setting, so a bad user key is
//!   `InvalidConfig` on `user_key` and a bad application token is `AuthFailed`
//!   on `api_key` — Sonarr blames `ApiKey` for both, and only during `Test`;
//! * the message body is enriched per event from the structured blocks the
//!   contract carries (episode, quality, release, indexer, client, size, paths,
//!   health, versions) rather than being `summary_message` alone;
//! * the API's documented limits are enforced *here*, with a `warnings` entry,
//!   instead of letting Pushover reject the message: message 1024, title 250,
//!   url 512, url_title 100 (<https://pushover.net/api>);
//! * `timestamp` is set from the contract's `occurred_at` so the notification
//!   carries the time the event happened rather than the time it was delivered,
//!   and `url`/`url_title` open the title on its metadata site. Sonarr sends
//!   neither.
//!
//! # Why the delivery path is local rather than `notify_common::send_form`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")`. Pushover's failures are three different lanes in Scryer's
//! contract: a 4xx is documented as permanent ("repeating your same request will
//! not work, no matter how many times you retry it") and names the offending
//! parameter, a 429 is the account's monthly quota with an `X-Limit-App-Reset`
//! the core can act on, and a 5xx is explicitly retryable after five seconds.
//!
//! # End-to-end encryption
//!
//! `encryption_key` keeps Sonarr's e2ee support (`PushoverProxy.cs:100-131`):
//! gzip, AES-256-CBC/PKCS7 under a random 16-byte IV, HMAC-SHA256 over IV‖
//! ciphertext, base64 of IV‖ciphertext‖MAC. Pushover documents `message`,
//! `title`, `url` and `url_title` as the encryptable fields, so all four are
//! encrypted when the key is set rather than only the two Sonarr encrypts.
//! Because Pushover cannot measure a field it cannot read, its length limits are
//! assumed to apply to the *encoded* value: `title` and `message` are shrunk
//! until the ciphertext fits, and `url_title` is dropped (its 100-character
//! limit is below the ~108-character floor of any encrypted field).
//!
//! # Upstream reference
//!
//! Read 2026-09-01, <https://pushover.net/api>: the Messages API parameter and
//! limit table, "Being Friendly to our API" (4xx permanent / 5xx retry after 5s
//! / two concurrent connections), "Message Limits" (`X-Limit-App-Limit`,
//! `X-Limit-App-Remaining`, `X-Limit-App-Reset`), the sound list, and the
//! end-to-end encryption section. Also
//! <https://blog.pushover.net/posts/2026/4/app-limits> (8 April 2026): from
//! 1 May 2026 the free 10,000/month quota and the `X-Limit-App-*` headers are
//! **per account**, shared by every application, not per application.

use std::collections::BTreeMap;
use std::io::Write;

use aes::Aes256;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use cbc::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
use flate2::{Compression, write::GzEncoder};
use hmac::{Hmac, KeyInit, Mac};
use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, PluginNotificationEpisode,
    current_sdk_constraint,
};
use serde_json::Value;
use sha2::Sha256;

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:notification/notification@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = build_descriptor,
    handler = handle_notification_command,
);

/// The `application/x-www-form-urlencoded` parameters of one Messages API call.
type FormParams = Vec<(String, String)>;

const PROVIDER_TYPE: &str = "pushover";
const USER_AGENT: &str = concat!("scryer-pushover-plugin/", env!("CARGO_PKG_VERSION"));

const PUSHOVER_API_BASE: &str = "https://api.pushover.net";
const PUSHOVER_API_HOST: &str = "api.pushover.net";
const PUSHOVER_MESSAGES_URL: &str = "https://api.pushover.net/1/messages.json";

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Pushover's documented limits (<https://pushover.net/api>)
// ---------------------------------------------------------------------------

/// "your message […] limited to 1024 4-byte UTF-8 characters".
const MESSAGE_CHARACTER_LIMIT: usize = 1024;
/// "your message's title […] limited to 250 characters".
const TITLE_CHARACTER_LIMIT: usize = 250;
/// "a supplementary URL […] limited to 512 characters".
const URL_CHARACTER_LIMIT: usize = 512;
/// "a title for the URL […] otherwise just the URL is shown; limited to 100
/// characters".
const URL_TITLE_CHARACTER_LIMIT: usize = 100;
/// "device name […] up to 25 characters, `[A-Za-z0-9_-]`".
const DEVICE_NAME_LIMIT: usize = 25;

/// A line shorter than this cannot carry a useful truncated value, so it is
/// dropped instead of being reduced to a label and an ellipsis.
const MIN_TRUNCATED_LINE: usize = 8;

const EMERGENCY_PRIORITY: i64 = 2;
/// "retry […] must have a value of at least 30 seconds between retries".
const MIN_RETRY_SECONDS: i64 = 30;
/// "expire […] must have a maximum value of at most 10800 seconds (3 hours)".
const MAX_EXPIRE_SECONDS: i64 = 10_800;
/// Pushover states no explicit floor for `expire`, but an emergency message that
/// expires before its first retry cannot be acknowledged, so the retry floor is
/// the only meaningful one. Sonarr's validator never checks `expire` at all
/// (`PushoverSettings.cs:14-15` applies the `Retry` rule twice).
const MIN_EXPIRE_SECONDS: i64 = 30;

/// "If you receive an HTTP 500 […] you can repeat your same request again, but
/// no sooner than 5 seconds".
const SERVER_ERROR_RETRY_SECONDS: i64 = 5;

/// The floor on the encoded length of *any* encrypted field: gzip of a single
/// byte is ~20 bytes, AES-CBC pads that to 32, and base64 of IV(16) ‖ ct(32) ‖
/// mac(32) is 108 characters. Anything with a smaller limit than this cannot be
/// sent encrypted at all.
const ENCRYPTED_FIELD_FLOOR: usize = 108;

/// The link shown on a test message when the request carries no title, standing
/// in for Sonarr's constant test body (`PushoverProxy.cs:86-87`).
const SCRYER_LINK: &str = "https://github.com/scryer-media/scryer";

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `PushoverPriority.cs`. Sonarr renders these as a select; the June port made
/// the field a free `Number`, so an operator could store `7` and have it
/// silently delivered as an invalid parameter. The stored values are unchanged.
const PRIORITY_OPTIONS: &[(&str, &str)] = &[
    ("-2", "Silent"),
    ("-1", "Quiet"),
    ("0", "Normal"),
    ("1", "High"),
    ("2", "Emergency"),
];

const METADATA_LINK_AUTO: &str = "auto";
const METADATA_LINK_NONE: &str = "none";

/// Which site `url`/`url_title` should open. Sonarr's Pushover sends no link at
/// all; `auto` picks the best id the title actually carries for its facet.
const METADATA_LINK_OPTIONS: &[(&str, &str)] = &[
    (METADATA_LINK_AUTO, "Automatic"),
    (METADATA_LINK_NONE, "None"),
    ("imdb", "IMDb"),
    ("tvdb", "TVDb"),
    ("tvmaze", "TVMaze"),
    ("trakt", "Trakt"),
    ("tmdb", "TMDb"),
    ("anidb", "AniDB"),
    ("anilist", "AniList"),
    ("mal", "MyAnimeList"),
    ("kitsu", "Kitsu"),
];

/// Preference order for `auto`, per facet. Episodic libraries lead with the id
/// Scryer is most likely to hold for a series; everything else leads with TMDb.
const AUTO_LINK_EPISODIC: &[&str] = &["tvdb", "tvmaze", "imdb", "tmdb", "anidb"];
const AUTO_LINK_OTHER: &[&str] = &["tmdb", "imdb", "tvdb"];

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Pushover".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // Fixed by the product: every call is against api.pushover.net.
            // Documentation rather than a prefill — nothing auto-provisions a
            // notification channel.
            default_base_url: Some(PUSHOVER_API_BASE.to_string()),
            allowed_hosts: vec![PUSHOVER_API_HOST.to_string()],
            capabilities: NotificationCapabilities {
                // Pushover has `html=1` and `monospace=1`, but the 1024-character
                // limit is measured on the parameter Pushover receives, so every
                // markup byte is a byte of content lost. Plain text carries more
                // of the message, which is what the operator is reading.
                supports_rich_text: false,
                // `attachment_base64` exists, but the contract carries a poster
                // *URL*, not bytes, and fetching it would need egress to an
                // arbitrary image host that `allowed_hosts` cannot express.
                supports_images: false,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![NotificationDeliveryMode::Push],
                payload_formats: vec![NotificationPayloadFormat::PlainText],
                supported_events: general_notification_events(),
                // Every event below renders distinctly, so all three of the
                // core's per-event filters are meaningful for this channel.
                event_options: NotificationEventOptions {
                    supports_upgrade_filter: true,
                    supports_delete_for_upgrade_filter: true,
                    supports_health_warning_filter: true,
                },
            },
            config_fields: config_fields(),
        }),
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("The Pushover application token, from https://pushover.net/apps."),
        ),
        field(
            "user_key",
            "User Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("The Pushover user or group key that receives the notification."),
        ),
        // Sonarr models this as a `Tag` (`PushoverSettings.cs:38`). Scryer's
        // notification settings UI renders a `Tag` field as comma-separated
        // text, so the stored value is unchanged and existing configurations
        // keep parsing.
        field(
            "devices",
            "Devices",
            ConfigFieldType::Tag,
            false,
            None,
            Some(
                "Pushover device names to target; leave empty for all of the user's devices. Each name is at most 25 characters of letters, digits, underscores and hyphens.",
            ),
        ),
        select_field("priority", "Priority", Some("0"), PRIORITY_OPTIONS),
        field(
            "retry",
            "Retry",
            ConfigFieldType::Number,
            false,
            Some("0"),
            Some(
                "Emergency priority only: seconds between repeats. At least 30, and at most the expire window.",
            ),
        ),
        field(
            "expire",
            "Expire",
            ConfigFieldType::Number,
            false,
            Some("0"),
            Some(
                "Emergency priority only: seconds to keep repeating until acknowledged. Between 30 and 10800.",
            ),
        ),
        field(
            "ttl",
            "TTL",
            ConfigFieldType::Number,
            false,
            Some("0"),
            Some(
                "Seconds after which Pushover deletes the message from the user's devices. Zero keeps it until it is deleted by hand.",
            ),
        ),
        field(
            "sound",
            "Sound",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "A Pushover sound identifier, or a custom sound you uploaded. Empty uses the device's default. See https://pushover.net/api#sounds.",
            ),
        ),
        select_field(
            "metadata_link",
            "Metadata Link",
            Some(METADATA_LINK_AUTO),
            METADATA_LINK_OPTIONS,
        ),
        field(
            "encryption_key",
            "Encryption Key",
            ConfigFieldType::Password,
            false,
            None,
            Some(
                "A 64-character hexadecimal key matching the one configured in the Pushover app, for end-to-end encrypted notifications. See https://pushover.net/api#e2ee.",
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Everything the renderer and the sender need from configuration, resolved and
/// validated once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    api_key: String,
    user_key: String,
    devices: Vec<String>,
    priority: i64,
    retry: i64,
    expire: i64,
    ttl: i64,
    sound: Option<String>,
    metadata_link: String,
    encryption_key: Option<[u8; 32]>,
}

impl Settings {
    /// `strict` is the Test-time posture. Rules Pushover itself will enforce
    /// (priority range, the emergency retry/expire window, a malformed
    /// encryption key) are errors on every send, because letting them through
    /// produces a guaranteed 4xx. Rules that are only *probably* wrong — a
    /// device name outside Pushover's documented character set — are refused at
    /// Test time and degraded to a warning on a live send, so a channel that
    /// works today keeps working if Pushover ever widens the rule.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let mut warnings = Vec::new();

        let api_key = required_config("api_key").map_err(config_error)?;
        let user_key = required_config("user_key").map_err(config_error)?;

        let priority = parse_number("priority", 0)?;
        if !(-2..=2).contains(&priority) {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("priority must be between -2 (Silent) and 2 (Emergency); got {priority}"),
                None,
            ));
        }

        let retry = parse_number("retry", 0)?;
        let expire = parse_number("expire", 0)?;
        if priority == EMERGENCY_PRIORITY {
            // `PushoverSettings.cs:14`, corrected against the API: Sonarr's
            // upper bound of 86400 cannot be reached, because a retry longer
            // than the 10800-second expire ceiling never fires twice.
            if !(MIN_RETRY_SECONDS..=MAX_EXPIRE_SECONDS).contains(&retry) {
                return Err(plugin_error(
                    PluginErrorCode::InvalidConfig,
                    format!(
                        "retry must be between {MIN_RETRY_SECONDS} and {MAX_EXPIRE_SECONDS} seconds when priority is Emergency; got {retry}"
                    ),
                    None,
                ));
            }
            // The rule Sonarr's validator never applies: it repeats the `Retry`
            // rule instead of writing an `Expire` one (`PushoverSettings.cs:15`).
            if !(MIN_EXPIRE_SECONDS..=MAX_EXPIRE_SECONDS).contains(&expire) {
                return Err(plugin_error(
                    PluginErrorCode::InvalidConfig,
                    format!(
                        "expire must be between {MIN_EXPIRE_SECONDS} and {MAX_EXPIRE_SECONDS} seconds when priority is Emergency; got {expire}"
                    ),
                    None,
                ));
            }
            if retry > expire {
                warnings.push(format!(
                    "retry ({retry}s) is longer than expire ({expire}s): the emergency alert will repeat at most once"
                ));
            }
        }

        let ttl = parse_number("ttl", 0)?;
        if ttl < 0 {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("ttl must be zero or a positive number of seconds; got {ttl}"),
                None,
            ));
        }
        if ttl > 0 && priority == EMERGENCY_PRIORITY {
            warnings.push(
                "ttl is ignored for emergency-priority messages, which live until they are acknowledged".to_string(),
            );
        }

        let devices = validated_devices(&config_csv("devices"), strict, &mut warnings)?;
        let metadata_link = validated_metadata_link(config_value("metadata_link").as_deref())?;
        let encryption_key = parse_encryption_key(config_value("encryption_key").as_deref())?;

        Ok((
            Self {
                api_key,
                user_key,
                devices,
                priority,
                retry,
                expire,
                ttl,
                sound: config_value("sound"),
                metadata_link,
                encryption_key,
            },
            warnings,
        ))
    }
}

/// A number setting that will not parse is a configuration error, not a zero.
///
/// `config_i64` silently substitutes the default, so the June port turned
/// `priority = "high"` into Normal and `retry = "5m"` into an invalid emergency
/// message, both without telling anyone.
fn parse_number(key: &'static str, default_value: i64) -> Result<i64, PluginError> {
    let Some(raw) = config_value(key) else {
        return Ok(default_value);
    };
    raw.parse::<i64>().map_err(|error| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("{key} must be a whole number; got {raw:?}"),
            Some(error.to_string()),
        )
    })
}

/// Pushover documents a device name as at most 25 characters of
/// `[A-Za-z0-9_-]`, and rejects the whole message when one is wrong.
fn validated_devices(
    devices: &[String],
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, PluginError> {
    let mut valid = Vec::new();
    for device in devices {
        let device = device.trim();
        if device.is_empty() {
            continue;
        }
        let well_formed = device.chars().count() <= DEVICE_NAME_LIMIT
            && device.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            });
        if !well_formed {
            if strict {
                return Err(plugin_error(
                    PluginErrorCode::InvalidConfig,
                    format!(
                        "devices contains {device:?}, which is not a Pushover device name (at most {DEVICE_NAME_LIMIT} characters of letters, digits, underscores and hyphens)"
                    ),
                    None,
                ));
            }
            warnings.push(format!(
                "devices entry {device:?} does not look like a Pushover device name; Pushover may reject the message"
            ));
        }
        if !valid.iter().any(|existing| existing == device) {
            valid.push(device.to_string());
        }
    }
    Ok(valid)
}

fn validated_metadata_link(raw: Option<&str>) -> Result<String, PluginError> {
    let value = raw
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| METADATA_LINK_AUTO.to_string());
    if !METADATA_LINK_OPTIONS.iter().any(|(key, _)| *key == value) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("metadata_link is not a valid value: {value}"),
            Some(format!(
                "known values: {}",
                METADATA_LINK_OPTIONS
                    .iter()
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        ));
    }
    Ok(value)
}

/// `PushoverSettingsValidator` (`PushoverSettings.cs:17`):
/// `^[0-9a-fA-F]{64}$`. Sonarr reports this through its settings form; here it
/// is a typed `InvalidConfig` on every send, because the June port answered a
/// malformed key as a *delivery failure*.
fn parse_encryption_key(raw: Option<&str>) -> Result<Option<[u8; 32]>, PluginError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let malformed = || {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            "encryption_key must be a 64-character hexadecimal string".to_string(),
            Some(format!(
                "configured value is {} characters",
                raw.chars().count()
            )),
        )
    };
    if raw.len() != 64 || !raw.is_ascii() {
        return Err(malformed());
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&raw[start..start + 2], 16).map_err(|_| malformed())?;
    }
    Ok(Some(bytes))
}

// ---------------------------------------------------------------------------
// Message model
//
// Pushover caps `message` at 1024 characters and `title` at 250, and rejects
// anything longer outright. The message is built as typed lines whose visible
// length is known before rendering, so the tail can be trimmed with a warning
// rather than the whole notification being lost.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Plain(String),
    Labeled(&'static str, String),
}

impl Line {
    fn render(&self) -> String {
        match self {
            Line::Plain(text) => text.clone(),
            Line::Labeled(label, value) => format!("{label}: {value}"),
        }
    }

    fn visible_len(&self) -> usize {
        match self {
            Line::Plain(text) => char_count(text),
            Line::Labeled(label, value) => char_count(label) + 2 + char_count(value),
        }
    }

    /// The same line reduced to `budget` visible characters, or `None` when the
    /// budget cannot hold anything worth sending.
    fn truncated_to(&self, budget: usize) -> Option<Line> {
        if budget < MIN_TRUNCATED_LINE {
            return None;
        }
        match self {
            Line::Plain(text) => Some(Line::Plain(ellipsize(text, budget))),
            Line::Labeled(label, value) => {
                let room = budget.checked_sub(char_count(label) + 2)?;
                (room >= 4).then(|| Line::Labeled(label, ellipsize(value, room)))
            }
        }
    }
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn ellipsize(text: &str, budget: usize) -> String {
    if char_count(text) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Render the lines into the message body, dropping the tail that does not fit.
///
/// The summary comes first and the enrichment last, so trimming from the end
/// degrades detail rather than meaning.
fn render_message(lines: &[Line]) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let mut rendered: Vec<String> = Vec::new();
    let mut used = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let separator = usize::from(!rendered.is_empty());
        if used + separator + line.visible_len() <= MESSAGE_CHARACTER_LIMIT {
            used += separator + line.visible_len();
            rendered.push(line.render());
            continue;
        }

        let budget = MESSAGE_CHARACTER_LIMIT.saturating_sub(used + separator);
        let mut dropped = lines.len() - index;
        if let Some(shortened) = line.truncated_to(budget) {
            rendered.push(shortened.render());
            dropped -= 1;
        }
        let detail = if dropped > 0 {
            format!(" ({dropped} line(s) dropped)")
        } else {
            String::new()
        };
        warnings.push(format!(
            "message trimmed to Pushover's {MESSAGE_CHARACTER_LIMIT}-character limit{detail}"
        ));
        break;
    }

    (rendered.join("\n"), warnings)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Sonarr sends a fixed constant per event ("Episode Grabbed", "Import
/// Complete", …) and puts everything else in the body (`Pushover.cs:19-67`).
/// Scryer's dispatcher already composes an event-specific, title-bearing heading
/// in `summary_title` ("Grabbed: Example Show"), which is strictly more
/// informative in a push notification whose title is what the lock screen shows.
fn heading(req: &PluginNotificationRequest) -> String {
    let title = req.summary_title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let app = req.app.name.trim();
    if app.is_empty() {
        "Scryer".to_string()
    } else {
        app.to_string()
    }
}

fn build_lines(req: &PluginNotificationRequest) -> Vec<Line> {
    let mut lines = Vec::new();

    let message = req.summary_message.trim();
    if !message.is_empty() {
        lines.push(Line::Plain(message.to_string()));
    }

    lines.extend(detail_lines(req));

    // Pushover's message must not be empty; a request whose summary and blocks
    // are all blank still has a heading.
    if lines.is_empty() {
        lines.push(Line::Plain(heading(req)));
    }

    lines
}

/// The structured enrichment Sonarr's Pushover channel has no room for: Sonarr
/// hands the proxy one prose sentence, while Scryer's contract carries the facts
/// separately. Every line is conditional on the block actually being present, so
/// the sparse shape the core sends today renders exactly the one line the June
/// port sent.
fn detail_lines(req: &PluginNotificationRequest) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    match req.event_type {
        NotificationEventType::Grab => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Release Group", release_group(req));
            push(&mut lines, "Indexer", indexer(req));
            push(&mut lines, "Size", size(req));
            push(&mut lines, "Client", client_name(req));
        }
        // `NotificationEventType::Download` only ever carries a FAILED download:
        // the dispatcher maps `DownloadFailed` onto it
        // (`crates/scryer-application/src/notifications/dispatcher.rs:34,418-448`,
        // release-0.19.8). A successful import is `ImportComplete`/`Upgrade`, so
        // this arm renders a failure and never an import path.
        NotificationEventType::Download => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Status", download_status(req));
        }
        NotificationEventType::ImportComplete
        | NotificationEventType::Upgrade
        | NotificationEventType::PostProcessingCompleted => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Release Group", release_group(req));
            push(&mut lines, "Size", size(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Destination", import_path(req));
        }
        NotificationEventType::ImportRejected => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Source", source_path(req));
            push(&mut lines, "Status", import_status(req));
        }
        NotificationEventType::Rename => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", primary_path(req));
        }
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", deleted_path(req));
            push(&mut lines, "Quality", quality(req));
        }
        NotificationEventType::TitleAdded | NotificationEventType::TitleDeleted => {
            push(&mut lines, "Path", title_path(req));
        }
        NotificationEventType::HealthIssue | NotificationEventType::HealthRestored => {
            push(&mut lines, "Check", health_source(req));
            push(&mut lines, "Detail", health_detail(req));
        }
        NotificationEventType::ApplicationUpdate => {
            push(
                &mut lines,
                "Previous Version",
                application_version(req, false),
            );
            push(&mut lines, "New Version", application_version(req, true));
        }
        NotificationEventType::ManualInteractionRequired => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Download", download_title(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Reason", manual_reason(req));
        }
        NotificationEventType::SubtitleDownloaded | NotificationEventType::SubtitleSearchFailed => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", primary_path(req));
            push(&mut lines, "Languages", subtitle_languages(req));
        }
        NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestApproved
        | NotificationEventType::MediaRequestRejected
        | NotificationEventType::MediaRequestCanceled => {
            push(&mut lines, "Status", media_request_status(req));
            push(&mut lines, "Quality Profile", media_request_profile(req));
        }
        NotificationEventType::Test => {}
    }
    lines
}

fn push(lines: &mut Vec<Line>, label: &'static str, value: Option<String>) {
    if let Some(value) = value.map(|value| value.trim().to_string())
        && !value.is_empty()
    {
        lines.push(Line::Labeled(label, value));
    }
}

// ---------------------------------------------------------------------------
// Field values
// ---------------------------------------------------------------------------

/// The contract's rendered `episode.display` when the core filled it, otherwise
/// composed the way Sonarr composes an episode heading: `{season}x{episode}` and
/// the episode titles, or the air date for a daily episode.
fn episode_display(req: &PluginNotificationRequest) -> Option<String> {
    if let Some(display) = req
        .episode
        .as_ref()
        .and_then(|episode| episode.display.as_deref())
        .map(str::trim)
        .filter(|display| !display.is_empty())
    {
        return Some(display.to_string());
    }

    let episodes: Vec<&PluginNotificationEpisode> = if req.episodes.is_empty() {
        req.episode.iter().collect()
    } else {
        req.episodes.iter().collect()
    };
    let first = episodes.first().copied()?;

    let titles = episodes
        .iter()
        .filter_map(|episode| episode.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .collect::<Vec<_>>()
        .join(" + ");

    if first.episode_number.is_none()
        && let Some(air_date) = first
            .air_date
            .as_deref()
            .map(str::trim)
            .filter(|air_date| !air_date.is_empty())
    {
        return Some(if titles.is_empty() {
            air_date.to_string()
        } else {
            format!("{air_date} - {titles}")
        });
    }

    let numbers: String = episodes
        .iter()
        .filter_map(|episode| episode.episode_number.as_deref())
        .map(str::trim)
        .filter(|number| !number.is_empty())
        .map(|number| match number.parse::<u32>() {
            Ok(parsed) => format!("x{parsed:02}"),
            Err(_) => format!("x{number}"),
        })
        .collect();

    let season = first
        .season_number
        .as_deref()
        .map(str::trim)
        .filter(|season| !season.is_empty());

    match (season, numbers.is_empty(), titles.is_empty()) {
        (Some(season), false, true) => Some(format!("{season}{numbers}")),
        (Some(season), false, false) => Some(format!("{season}{numbers} - {titles}")),
        (_, _, false) => Some(titles),
        _ => None,
    }
}

fn quality(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.quality.clone()),
    )
    .or_else(|| {
        req.media_files
            .iter()
            .find_map(|file| non_empty(file.quality.clone()))
    })
}

fn release_title(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.source_title.clone()),
    )
    .or_else(|| {
        non_empty(
            req.import
                .as_ref()
                .and_then(|import| import.source_title.clone()),
        )
    })
    .or_else(|| {
        non_empty(
            req.download
                .as_ref()
                .and_then(|download| download.title.clone()),
        )
    })
}

fn release_group(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.release_group.clone()),
    )
    .or_else(|| {
        req.media_files
            .iter()
            .find_map(|file| non_empty(file.release_group.clone()))
    })
}

fn indexer(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.indexer.clone()),
    )
    .or_else(|| {
        non_empty(
            req.release
                .as_ref()
                .and_then(|release| release.provider.clone()),
        )
    })
}

fn size(req: &PluginNotificationRequest) -> Option<String> {
    let bytes = req
        .download
        .as_ref()
        .and_then(|download| download.size_bytes)
        .filter(|bytes| *bytes > 0)
        .or_else(|| {
            let total: i64 = req
                .media_files
                .iter()
                .filter_map(|file| file.size_bytes)
                .sum();
            (total > 0).then_some(total)
        })?;
    Some(format_bytes(bytes))
}

fn client_name(req: &PluginNotificationRequest) -> Option<String> {
    let download = req.download.as_ref()?;
    non_empty(download.client_name.clone()).or_else(|| non_empty(download.client_type.clone()))
}

fn download_title(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.download
            .as_ref()
            .and_then(|download| download.title.clone()),
    )
}

fn download_status(req: &PluginNotificationRequest) -> Option<String> {
    let download = req.download.as_ref()?;
    non_empty(download.status_message.clone()).or_else(|| non_empty(download.status.clone()))
}

fn import_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.import
            .as_ref()
            .and_then(|import| import.dest_path.clone()),
    )
    .or_else(|| primary_path(req))
}

fn source_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.import
            .as_ref()
            .and_then(|import| import.source_path.clone()),
    )
}

fn import_status(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.import.as_ref().and_then(|import| import.status.clone()))
}

fn primary_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.file.as_ref().and_then(|file| file.primary_path.clone())).or_else(|| {
        req.file.as_ref().and_then(|file| {
            file.media_updates
                .first()
                .map(|update| update.path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
    })
}

/// The core puts the deleted path first in `file.media_updates`; `import`
/// carries an explicit list when the delete is an upgrade replacement.
fn deleted_path(req: &PluginNotificationRequest) -> Option<String> {
    req.file
        .as_ref()
        .and_then(|file| {
            file.media_updates
                .iter()
                .find(|update| {
                    update.update_type == scryer_plugin_sdk::NotificationMediaUpdateType::Deleted
                })
                .map(|update| update.path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
        .or_else(|| {
            req.import.as_ref().and_then(|import| {
                import
                    .deleted_paths
                    .first()
                    .map(|path| path.trim().to_string())
                    .filter(|path| !path.is_empty())
            })
        })
        .or_else(|| primary_path(req))
}

fn title_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.title.as_ref().and_then(|title| title.path.clone()))
}

fn health_source(req: &PluginNotificationRequest) -> Option<String> {
    let health = req.health.as_ref()?;
    non_empty(health.code.clone()).or_else(|| non_empty(health.status.clone()))
}

fn health_detail(req: &PluginNotificationRequest) -> Option<String> {
    let health = req.health.as_ref()?;
    non_empty(health.details.clone()).or_else(|| non_empty(health.message.clone()))
}

fn application_version(req: &PluginNotificationRequest, target: bool) -> Option<String> {
    let update = req.application_update.as_ref()?;
    non_empty(if target {
        update.target_version.clone()
    } else {
        update.current_version.clone()
    })
}

fn manual_reason(req: &PluginNotificationRequest) -> Option<String> {
    let interaction = req.manual_interaction.as_ref()?;
    non_empty(interaction.reason.clone()).or_else(|| non_empty(interaction.kind.clone()))
}

/// Only an absolute http(s) link is offered to Pushover: `url` is a tap target
/// on the device, and a relative path is a dead one.
fn manual_link(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.manual_interaction
            .as_ref()
            .and_then(|interaction| interaction.link.clone()),
    )
    .filter(|link| is_absolute_http(link))
}

fn is_absolute_http(link: &str) -> bool {
    let link = link.to_ascii_lowercase();
    link.starts_with("http://") || link.starts_with("https://")
}

fn subtitle_languages(req: &PluginNotificationRequest) -> Option<String> {
    let languages: Vec<String> = req
        .media_files
        .iter()
        .flat_map(|file| file.subtitle_languages.iter())
        .map(|language| language.trim().to_string())
        .filter(|language| !language.is_empty())
        .collect();
    (!languages.is_empty()).then(|| languages.join(", "))
}

fn media_request_status(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.media_request
            .as_ref()
            .and_then(|request| request.status.clone()),
    )
}

fn media_request_profile(req: &PluginNotificationRequest) -> Option<String> {
    let request = req.media_request.as_ref()?;
    non_empty(request.approved_quality_profile_name.clone())
        .or_else(|| non_empty(request.requested_quality_profile_name.clone()))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Sonarr's `BytesToString` rounding, so sizes read the same across channels.
fn format_bytes(bytes: i64) -> String {
    const SUFFIXES: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let magnitude = bytes.unsigned_abs() as f64;
    let place = (magnitude.log(1024.0).floor() as i32).clamp(0, 6);
    let scaled = magnitude / 1024f64.powi(place);
    let rounded = (scaled * 10.0).round() / 10.0;
    let signed = if bytes < 0 { -rounded } else { rounded };
    format!("{} {}", signed, SUFFIXES[place as usize])
}

// ---------------------------------------------------------------------------
// Supplementary URL
// ---------------------------------------------------------------------------

/// The `url`/`url_title` pair Pushover shows under the message.
///
/// Sonarr's Pushover sends neither, so tapping a notification opens nothing.
/// `ManualInteractionRequired` carries its own deep link into Scryer and wins;
/// otherwise the operator's chosen metadata site (or the best id the title
/// carries, under `auto`) opens the title.
fn supplementary_link(
    req: &PluginNotificationRequest,
    settings: &Settings,
) -> Option<(String, String)> {
    if let Some(link) = manual_link(req) {
        return Some((link, format!("Open in {}", req.app.name.trim())));
    }

    if settings.metadata_link != METADATA_LINK_NONE
        && let Some((label, url)) = metadata_link(req, &settings.metadata_link)
    {
        let name = req
            .title
            .as_ref()
            .map(|title| title.name.trim())
            .filter(|name| !name.is_empty());
        return Some(match name {
            Some(name) => (url, format!("{name} on {label}")),
            None => (url, label.to_string()),
        });
    }

    // Sonarr's test body names the product; a test that carries a working link
    // also proves the `url` pair renders on the device.
    if req.is_test {
        return Some((SCRYER_LINK.to_string(), req.app.name.trim().to_string()));
    }

    None
}

/// `NotificationMetadataLinkGenerator.GenerateLinks` on Scryer's contract,
/// narrowed to the one slot Pushover has.
///
/// The facet decides what "Trakt" and "TMDb" mean, which is the part Sonarr's
/// series-only model cannot express.
fn metadata_link(req: &PluginNotificationRequest, choice: &str) -> Option<(&'static str, String)> {
    let title = req.title.as_ref()?;
    let ids = &title.external_ids;
    let episodic = matches!(
        title.facet.to_ascii_lowercase().as_str(),
        "series" | "anime" | "tv" | "show"
    );

    if choice == METADATA_LINK_AUTO {
        let order = if episodic {
            AUTO_LINK_EPISODIC
        } else {
            AUTO_LINK_OTHER
        };
        return order.iter().find_map(|key| link_for(key, ids, episodic));
    }

    link_for(choice, ids, episodic)
}

fn link_for(
    key: &str,
    ids: &scryer_plugin_sdk::PluginNotificationExternalIds,
    episodic: bool,
) -> Option<(&'static str, String)> {
    let imdb = external_id(ids.imdb_id.as_deref(), ids, "imdb");
    let tvdb = external_id(ids.tvdb_id.as_deref(), ids, "tvdb");
    let tmdb = external_id(ids.tmdb_id.as_deref(), ids, "tmdb");
    let tvmaze = external_id(ids.tvmaze_id.as_deref(), ids, "tvmaze");

    // Sonarr's `http://` URLs are emitted as `https://`: every one of these
    // sites redirects, and an http link on a phone is a needless hop.
    match key {
        "imdb" => imdb.map(|id| ("IMDb", format!("https://www.imdb.com/title/{id}"))),
        "tvdb" => tvdb.map(|id| ("TVDb", format!("https://thetvdb.com/?tab=series&id={id}"))),
        "tvmaze" => tvmaze.map(|id| ("TVMaze", format!("https://www.tvmaze.com/shows/{id}"))),
        "trakt" => {
            if episodic {
                tvdb.map(|id| {
                    (
                        "Trakt",
                        format!("https://trakt.tv/search/tvdb/{id}?id_type=show"),
                    )
                })
            } else {
                tmdb.map(|id| {
                    (
                        "Trakt",
                        format!("https://trakt.tv/search/tmdb/{id}?id_type=movie"),
                    )
                })
                .or_else(|| imdb.map(|id| ("Trakt", format!("https://trakt.tv/search/imdb/{id}"))))
            }
        }
        "tmdb" => tmdb.map(|id| {
            (
                "TMDb",
                if episodic {
                    format!("https://www.themoviedb.org/tv/{id}")
                } else {
                    format!("https://www.themoviedb.org/movie/{id}")
                },
            )
        }),
        "anidb" => external_id(ids.anidb_id.as_deref(), ids, "anidb")
            .map(|id| ("AniDB", format!("https://anidb.net/anime/{id}"))),
        "anilist" => external_id(ids.anilist_ids.first().map(String::as_str), ids, "anilist")
            .map(|id| ("AniList", format!("https://anilist.co/anime/{id}"))),
        "mal" => external_id(ids.mal_ids.first().map(String::as_str), ids, "mal")
            .map(|id| ("MyAnimeList", format!("https://myanimelist.net/anime/{id}"))),
        "kitsu" => external_id(ids.kitsu_ids.first().map(String::as_str), ids, "kitsu")
            .map(|id| ("Kitsu", format!("https://kitsu.app/anime/{id}"))),
        _ => None,
    }
}

fn external_id(
    typed: Option<&str>,
    ids: &scryer_plugin_sdk::PluginNotificationExternalIds,
    source: &str,
) -> Option<String> {
    typed
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(|| {
            ids.by_source
                .get(source)
                .and_then(|values| values.first())
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
        })
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// `timestamp` — "a Unix timestamp of your message's date and time to display
/// to the user, rather than the time your message is received by our API".
///
/// The dispatcher always stamps `occurred_at` with an RFC 3339 instant
/// (`dispatcher.rs:886`, `event.occurred_at.to_rfc3339()`), so the notification
/// can carry the time the event happened even when delivery is delayed. Sonarr
/// sends no timestamp at all.
fn unix_timestamp(value: &str) -> Option<i64> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 20 || !value.is_ascii() {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }

    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;

    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        // RFC 3339 permits a leap second.
        || second > 60
    {
        return None;
    }

    let mut rest = value.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = rest.get(1 + digits..)?;
    }

    let offset_minutes = if rest.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let body = rest.get(1..)?;
        let (hours, minutes) = match body.len() {
            5 if body.as_bytes()[2] == b':' => (body.get(0..2)?, body.get(3..5)?),
            4 => (body.get(0..2)?, body.get(2..4)?),
            _ => return None,
        };
        let hours: i64 = hours.parse().ok()?;
        let minutes: i64 = minutes.parse().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        sign * (hours * 60 + minutes)
    };

    Some(
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
            - offset_minutes * 60,
    )
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a proleptic
/// Gregorian date. Cheaper than a date crate for the one thing this plugin needs
/// a calendar for.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn now_unix() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// Encryption (PushoverProxy.cs:100-131, https://pushover.net/api#e2ee)
// ---------------------------------------------------------------------------

fn encrypt_field(plaintext: &str, key: &[u8; 32]) -> Result<String, String> {
    let mut iv = [0u8; 16];
    getrandom::fill(&mut iv)
        .map_err(|err| format!("failed to generate pushover encryption IV: {err}"))?;
    encrypt_field_with_iv(plaintext, key, &iv)
}

/// Split out from [`encrypt_field`] so the wire format can be pinned by a test
/// against a fixed IV instead of only being exercised through a random one.
fn encrypt_field_with_iv(plaintext: &str, key: &[u8; 32], iv: &[u8; 16]) -> Result<String, String> {
    let compressed = gzip_compress(plaintext.as_bytes())?;

    let ciphertext = cbc::Encryptor::<Aes256>::new(key.into(), iv.into())
        .encrypt_padded_vec::<Pkcs7>(&compressed);

    let mut iv_and_ciphertext = Vec::with_capacity(iv.len() + ciphertext.len());
    iv_and_ciphertext.extend_from_slice(iv);
    iv_and_ciphertext.extend_from_slice(&ciphertext);

    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|err| format!("failed to sign pushover payload: {err}"))?;
    mac.update(&iv_and_ciphertext);
    let signature = mac.finalize().into_bytes();

    let mut payload = Vec::with_capacity(iv_and_ciphertext.len() + signature.len());
    payload.extend_from_slice(&iv_and_ciphertext);
    payload.extend_from_slice(&signature);

    Ok(BASE64.encode(payload))
}

fn gzip_compress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(bytes)
        .map_err(|err| format!("failed to gzip pushover field: {err}"))?;
    encoder
        .finish()
        .map_err(|err| format!("failed to finish pushover gzip field: {err}"))
}

/// One field of the outgoing message, already cut to Pushover's limit.
///
/// Without a key this is `ellipsize` and nothing more. With one, the limit has
/// to be met by the *encoded* value: Pushover cannot measure a field it cannot
/// decrypt, so the only safe reading of "limited to 1024 characters" is that it
/// applies to what the API receives. The plaintext is therefore shrunk until the
/// ciphertext fits.
fn encode_field(
    plaintext: &str,
    key: Option<&[u8; 32]>,
    limit: usize,
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<String, PluginError> {
    let Some(key) = key else {
        if char_count(plaintext) > limit {
            warnings.push(format!(
                "{label} trimmed to Pushover's {limit}-character limit"
            ));
        }
        return Ok(ellipsize(plaintext, limit));
    };

    let mut budget = char_count(plaintext).min(limit);
    let mut reduced = false;
    // Bounded: each pass keeps 90% of the previous budget, so 1024 characters
    // reach zero in under 70 passes and every realistic field in a handful.
    for _ in 0..96 {
        let candidate = ellipsize(plaintext, budget);
        let encoded = encrypt_field(&candidate, key).map_err(|error| {
            plugin_error(
                PluginErrorCode::Temporary,
                format!("failed to encrypt the Pushover {label}"),
                Some(error),
            )
        })?;
        if char_count(&encoded) <= limit {
            if reduced || char_count(plaintext) > budget {
                warnings.push(format!(
                    "{label} trimmed to fit Pushover's {limit}-character limit once encrypted"
                ));
            }
            return Ok(encoded);
        }
        if budget == 0 {
            break;
        }
        budget = (budget * 9 / 10).min(budget - 1);
        reduced = true;
    }

    Err(plugin_error(
        PluginErrorCode::Permanent,
        format!("the encrypted Pushover {label} cannot be made to fit its {limit}-character limit"),
        None,
    ))
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// The form parameters of one `POST /1/messages.json`, in the order Sonarr
/// writes them (`PushoverProxy.cs:49-75`) plus the ones it never sends.
fn build_params(
    req: &PluginNotificationRequest,
    settings: &Settings,
) -> Result<(FormParams, Vec<String>), PluginError> {
    let key = settings.encryption_key.as_ref();

    let (body, mut warnings) = render_message(&build_lines(req));
    let title = encode_field(
        &heading(req),
        key,
        TITLE_CHARACTER_LIMIT,
        "title",
        &mut warnings,
    )?;
    let message = encode_field(
        &body,
        key,
        MESSAGE_CHARACTER_LIMIT,
        "message",
        &mut warnings,
    )?;

    let mut params = vec![
        ("token".to_string(), settings.api_key.clone()),
        ("user".to_string(), settings.user_key.clone()),
        ("title".to_string(), title),
        ("message".to_string(), message),
        ("priority".to_string(), settings.priority.to_string()),
    ];

    // Sonarr always sends `device`, even when empty (`PushoverProxy.cs:51`).
    // Pushover treats an empty `device` as "all devices", so the parameter is
    // omitted instead of sent blank.
    if !settings.devices.is_empty() {
        params.push(("device".to_string(), settings.devices.join(",")));
    }

    if key.is_some() {
        params.push(("encrypted".to_string(), "1".to_string()));
    }

    if settings.priority == EMERGENCY_PRIORITY {
        params.push(("retry".to_string(), settings.retry.to_string()));
        params.push(("expire".to_string(), settings.expire.to_string()));
    }

    if settings.ttl > 0 {
        params.push(("ttl".to_string(), settings.ttl.to_string()));
    }

    if let Some(sound) = settings.sound.as_ref() {
        params.push(("sound".to_string(), sound.clone()));
    }

    if let Some(timestamp) = req.occurred_at.as_deref().and_then(unix_timestamp) {
        params.push(("timestamp".to_string(), timestamp.to_string()));
    }

    if let Some((url, url_title)) = supplementary_link(req, settings) {
        if char_count(&url) > URL_CHARACTER_LIMIT {
            // A cut URL is a broken URL, so it is dropped rather than trimmed.
            warnings.push(format!(
                "supplementary url dropped: longer than Pushover's {URL_CHARACTER_LIMIT}-character limit"
            ));
        } else {
            let encoded_url = encode_field(&url, key, URL_CHARACTER_LIMIT, "url", &mut warnings)?;
            params.push(("url".to_string(), encoded_url));

            if key.is_some() {
                // Any encrypted field is at least 108 characters, so an
                // encrypted `url_title` can never satisfy the API's 100.
                warnings.push(format!(
                    "url_title omitted: an encrypted field is at least {ENCRYPTED_FIELD_FLOOR} characters and url_title is limited to {URL_TITLE_CHARACTER_LIMIT}"
                ));
            } else {
                params.push((
                    "url_title".to_string(),
                    ellipsize(&url_title, URL_TITLE_CHARACTER_LIMIT),
                ));
            }
        }
    }

    Ok((params, warnings))
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    let (params, payload_warnings) = match build_params(req, &settings) {
        Ok(built) => built,
        Err(error) => return PluginResult::Err(error),
    };
    warnings.extend(payload_warnings);

    let request = HttpRequest::new(PUSHOVER_MESSAGES_URL)
        .with_method("POST")
        .with_header("Content-Type", "application/x-www-form-urlencoded")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);

    match http::request::<Vec<u8>>(&request, Some(form_body(&params))) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            warnings,
        ),
        Err(error) => {
            // The host answers a refused or failed egress in-band; that is the
            // provider being unreachable, not a misconfigured channel.
            let mut failure = error_response(format!("request failed: {error}"), None);
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
    }
}

/// The JSON body every Messages API call returns
/// (<https://pushover.net/api>, "Response Format").
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PushoverBody {
    status: Option<i64>,
    request_id: Option<String>,
    receipt: Option<String>,
    errors: Vec<String>,
    /// The per-parameter keys Pushover adds alongside `errors`, e.g.
    /// `{"user":"invalid"}` or `{"token":"invalid"}`.
    invalid_parameters: Vec<String>,
    raw: Option<String>,
}

impl PushoverBody {
    fn detail(&self, status: u16) -> String {
        if !self.errors.is_empty() {
            return self.errors.join("; ");
        }
        match self.raw.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => ellipsize(raw, 300),
            _ => format!("HTTP {status}"),
        }
    }

    fn parameter_is_invalid(&self, name: &str) -> bool {
        self.invalid_parameters
            .iter()
            .any(|parameter| parameter == name)
    }

    fn mentions(&self, needle: &str) -> bool {
        self.errors
            .iter()
            .any(|error| error.to_ascii_lowercase().contains(needle))
    }
}

fn parse_pushover_body(body: &[u8]) -> PushoverBody {
    let text = String::from_utf8_lossy(body).to_string();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return PushoverBody {
            raw: Some(text),
            ..PushoverBody::default()
        };
    };

    let errors = map
        .get("errors")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Pushover flags the offending parameter by name with the value "invalid",
    // e.g. `{"user":"invalid","errors":["user identifier is invalid"],…}`.
    let invalid_parameters = map
        .iter()
        .filter(|(_, value)| value.as_str().is_some_and(|value| value == "invalid"))
        .map(|(key, _)| key.clone())
        .collect();

    PushoverBody {
        status: map.get("status").and_then(Value::as_i64),
        request_id: map
            .get("request")
            .and_then(Value::as_str)
            .map(str::to_string),
        receipt: map
            .get("receipt")
            .and_then(Value::as_str)
            .map(str::to_string),
        errors,
        invalid_parameters,
        raw: Some(text),
    }
}

/// Sonarr turns every Pushover failure into one `ValidationFailure("ApiKey", …)`
/// and only inside `Test` (`PushoverProxy.cs:82-98`); a live send just throws
/// into the log. Scryer's typed error lane exists on every send, so the operator
/// is always told which setting to fix.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    mut warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let answer = parse_pushover_body(body);
    let detail = answer.detail(status);

    if (200..300).contains(&status) && answer.status != Some(0) {
        let mut response = ok_response();
        // The emergency receipt is what the operator needs to poll or cancel the
        // alert; a normal message only has a request id.
        response.delivery_id = answer.receipt.clone().or_else(|| answer.request_id.clone());
        if let Some(warning) = quota_warning(headers) {
            warnings.push(warning);
        }
        response.warnings = warnings;
        return PluginResult::Ok(response);
    }

    match status {
        // "If you exceed your monthly message limit, you will receive an HTTP
        // 429". Since 1 May 2026 that limit is the whole account's, shared by
        // every application (blog.pushover.net/posts/2026/4/app-limits).
        429 => {
            let mut failure = error_response(
                format!(
                    "Pushover rejected the message: the account's monthly message limit is exhausted ({detail})"
                ),
                Some("http_429".to_string()),
            );
            failure.retry_after_seconds = quota_reset_seconds(headers);
            if let Some(warning) = quota_warning(headers) {
                warnings.push(warning);
            }
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        // "If you receive an HTTP 500 […] you can repeat your same request
        // again, but no sooner than 5 seconds."
        500..=599 => {
            let mut failure = error_response(
                format!("HTTP {status}: {detail}"),
                Some(format!("http_{status}")),
            );
            failure.retry_after_seconds = Some(SERVER_ERROR_RETRY_SECONDS);
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        // "If you receive an HTTP 4xx status […] repeating your same request
        // will not work, no matter how many times you retry it." Every 4xx below
        // is therefore a configuration or payload problem, never a delivery to
        // retry.
        _ => PluginResult::Err(classify_client_error(status, &answer, &detail)),
    }
}

fn classify_client_error(status: u16, answer: &PushoverBody, detail: &str) -> PluginError {
    let debug = format!("HTTP {status}: {detail}");

    // The application token: Pushover answers `{"token":"invalid"}`. Sonarr
    // blames this setting for everything; here it is the only thing it is
    // blamed for.
    if answer.parameter_is_invalid("token") || answer.mentions("application token") {
        return plugin_error(
            PluginErrorCode::AuthFailed,
            format!("api_key was rejected by Pushover: {detail}"),
            Some(debug),
        );
    }

    // "invalid user key or the user has no active devices".
    if answer.parameter_is_invalid("user") || answer.mentions("user identifier") {
        return plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "user_key was rejected by Pushover: {detail}. Check the user or group key, and that the account has at least one active device."
            ),
            Some(debug),
        );
    }

    for (parameter, setting) in [
        ("device", "devices"),
        ("sound", "sound"),
        ("retry", "retry"),
        ("expire", "expire"),
        ("ttl", "ttl"),
        ("priority", "priority"),
    ] {
        if answer.parameter_is_invalid(parameter) || answer.mentions(parameter) {
            return plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("{setting} was rejected by Pushover: {detail}"),
                Some(debug),
            );
        }
    }

    // The message this plugin built is wrong — too long, empty, or badly
    // encrypted. The operator has nothing to fix; this is a plugin bug and is
    // reported as one.
    if answer.mentions("message")
        || answer.mentions("title")
        || answer.mentions("url")
        || answer.mentions("encrypted")
    {
        return plugin_error(
            PluginErrorCode::Permanent,
            format!("Pushover rejected the message this plugin built: {detail}"),
            Some(debug),
        );
    }

    plugin_error(
        PluginErrorCode::InvalidConfig,
        format!("Pushover rejected the request (HTTP {status}): {detail}"),
        Some(debug),
    )
}

/// `X-Limit-App-Reset` is an absolute Unix timestamp (the first of next month),
/// not a delay, so it has to be turned into one. A proxy may add a plain
/// `Retry-After` instead; that one is already seconds.
fn quota_reset_seconds(headers: &BTreeMap<String, String>) -> Option<i64> {
    if let Some(seconds) =
        header(headers, "retry-after").and_then(|value| value.parse::<i64>().ok())
    {
        return Some(seconds.max(1));
    }
    let reset = header(headers, "x-limit-app-reset")?.parse::<i64>().ok()?;
    let now = now_unix()?;
    Some((reset - now).max(1))
}

/// Since 1 May 2026 the `X-Limit-App-*` headers "reflect the shared usage on the
/// account/team rather than the individual application", so a low remaining
/// count is a warning about every Scryer notification, not just this channel.
/// Surfaced through `warnings` so the operator learns about it before the
/// quota runs out and deliveries start failing with a 429.
fn quota_warning(headers: &BTreeMap<String, String>) -> Option<String> {
    let remaining = header(headers, "x-limit-app-remaining")?
        .parse::<i64>()
        .ok()?;
    let limit = header(headers, "x-limit-app-limit").and_then(|value| value.parse::<i64>().ok());
    let low = match limit {
        Some(limit) if limit > 0 => remaining * 20 <= limit,
        _ => remaining <= 100,
    };
    if !low {
        return None;
    }
    Some(match limit {
        Some(limit) => format!(
            "Pushover account message quota is nearly exhausted: {remaining} of {limit} messages remain this month"
        ),
        None => format!(
            "Pushover account message quota is nearly exhausted: {remaining} messages remain this month"
        ),
    })
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn plugin_error(
    code: PluginErrorCode,
    public_message: String,
    debug_message: Option<String>,
) -> PluginError {
    PluginError {
        code,
        public_message,
        debug_message,
        retry_after_seconds: None,
        details: None,
    }
}

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per operation this plugin exports. `action` is not one of them: the descriptor advertises no action, so the host does not route
/// one here and the arm answers **in-band** with `Unsupported` rather than
/// trapping. A trap under a component costs the whole instance and replaces the
/// plugin's own diagnosis with a generic ABI failure.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(send_notification(&request))
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockModeDecrypt;
    use flate2::read::GzDecoder;
    use scryer_plugin_sdk::{
        NotificationMediaUpdateType, NotificationSeverity, PluginNotificationApp,
        PluginNotificationApplicationUpdate, PluginNotificationDownload,
        PluginNotificationExternalIds, PluginNotificationFile, PluginNotificationHealth,
        PluginNotificationImport, PluginNotificationManualInteraction, PluginNotificationMediaFile,
        PluginNotificationMediaUpdate, PluginNotificationRelease, PluginNotificationTitle,
    };
    use std::io::Read;

    const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn settings() -> Settings {
        Settings {
            api_key: "apitoken".to_string(),
            user_key: "userkey".to_string(),
            devices: Vec::new(),
            priority: 0,
            retry: 0,
            expire: 0,
            ttl: 0,
            sound: None,
            metadata_link: METADATA_LINK_AUTO.to_string(),
            encryption_key: None,
        }
    }

    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: None,
            occurred_at: Some("2026-09-01T12:00:00+00:00".to_string()),
            correlation_id: None,
            actor: None,
            severity: Some(NotificationSeverity::Info),
            is_test: event_type == NotificationEventType::Test,
            summary_title: "Grabbed: Example Show".to_string(),
            summary_message: "Grabbed 'Example.Show.S01E01' for 'Example Show'.".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "0.19.8".to_string(),
            },
            title: None,
            episode: None,
            episodes: Vec::new(),
            release: None,
            download: None,
            import: None,
            health: None,
            file: None,
            media_files: Vec::new(),
            application_update: None,
            manual_interaction: None,
            media_request: None,
        }
    }

    fn series_title() -> PluginNotificationTitle {
        PluginNotificationTitle {
            id: Some("title-1".to_string()),
            name: "Example Show".to_string(),
            facet: "series".to_string(),
            year: Some(2024),
            slug: None,
            path: Some("/media/TV/Example Show".to_string()),
            overview: None,
            sort_title: None,
            background_url: None,
            poster_url: None,
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds {
                tvdb_id: Some("12345".to_string()),
                imdb_id: Some("tt0903747".to_string()),
                tvmaze_id: Some("82".to_string()),
                ..PluginNotificationExternalIds::default()
            },
        }
    }

    fn movie_title() -> PluginNotificationTitle {
        PluginNotificationTitle {
            facet: "movie".to_string(),
            name: "Example Movie".to_string(),
            external_ids: PluginNotificationExternalIds {
                tmdb_id: Some("603".to_string()),
                imdb_id: Some("tt0133093".to_string()),
                ..PluginNotificationExternalIds::default()
            },
            ..series_title()
        }
    }

    fn params_of(
        req: &PluginNotificationRequest,
        settings: &Settings,
    ) -> (BTreeMap<String, String>, Vec<String>) {
        let (params, warnings) = build_params(req, settings).expect("build the form parameters");
        (params.into_iter().collect(), warnings)
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    // -----------------------------------------------------------------
    // Descriptor
    // -----------------------------------------------------------------

    #[test]
    fn descriptor_keeps_every_june_config_key_and_fixes_the_two_field_types() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("pushover must describe a notification provider");
        };

        let keys: Vec<&str> = notification
            .config_fields
            .iter()
            .map(|field| field.key.as_str())
            .collect();
        for key in [
            "api_key",
            "user_key",
            "devices",
            "priority",
            "retry",
            "expire",
            "ttl",
            "sound",
            "encryption_key",
        ] {
            assert!(keys.contains(&key), "{key} must remain a config field");
        }
        assert!(keys.contains(&"metadata_link"));

        // `PushoverSettings.cs:38` — Sonarr's Devices is a Tag field.
        let devices = notification
            .config_fields
            .iter()
            .find(|field| field.key == "devices")
            .expect("devices");
        assert!(matches!(devices.field_type, ConfigFieldType::Tag));

        // `PushoverSettings.cs:41` — Sonarr's Priority is a select over
        // PushoverPriority.
        let priority = notification
            .config_fields
            .iter()
            .find(|field| field.key == "priority")
            .expect("priority");
        assert!(matches!(priority.field_type, ConfigFieldType::Select));
        assert_eq!(priority.default_value.as_deref(), Some("0"));
        let values: Vec<&str> = priority
            .options
            .iter()
            .map(|option| option.value.as_str())
            .collect();
        assert_eq!(values, vec!["-2", "-1", "0", "1", "2"]);
        assert_eq!(priority.options[0].label, "Silent");
        assert_eq!(priority.options[4].label, "Emergency");

        assert_eq!(
            notification.allowed_hosts,
            vec![PUSHOVER_API_HOST.to_string()]
        );
        assert_eq!(
            notification.default_base_url.as_deref(),
            Some(PUSHOVER_API_BASE)
        );
        assert!(!notification.capabilities.supports_rich_text);
        assert!(!notification.capabilities.supports_images);
        assert!(notification.capabilities.supports_test);
        assert!(
            notification
                .capabilities
                .event_options
                .supports_health_warning_filter
        );
    }

    // -----------------------------------------------------------------
    // Settings validation (H1)
    // -----------------------------------------------------------------

    #[test]
    fn an_unknown_metadata_link_names_its_field() {
        assert_eq!(validated_metadata_link(None).unwrap(), METADATA_LINK_AUTO);
        assert_eq!(validated_metadata_link(Some(" TVDb ")).unwrap(), "tvdb");

        let error = validated_metadata_link(Some("letterboxd")).expect_err("not an option");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("metadata_link"));
    }

    #[test]
    fn an_encryption_key_that_is_not_64_hex_characters_is_a_config_error() {
        assert!(parse_encryption_key(None).unwrap().is_none());
        assert!(parse_encryption_key(Some("   ")).unwrap().is_none());
        assert_eq!(
            parse_encryption_key(Some(TEST_KEY_HEX)).unwrap().unwrap()[0],
            0x00
        );
        assert_eq!(
            parse_encryption_key(Some(TEST_KEY_HEX)).unwrap().unwrap()[31],
            0x1f
        );

        for bad in ["abc", &"z".repeat(64), &"0".repeat(63), &"0".repeat(65)] {
            let error = parse_encryption_key(Some(bad)).expect_err("rejected");
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(
                error.public_message.contains("encryption_key"),
                "{}",
                error.public_message
            );
        }
    }

    #[test]
    fn device_names_follow_pushovers_documented_character_set() {
        let mut warnings = Vec::new();
        assert_eq!(
            validated_devices(
                &[
                    "phone".to_string(),
                    " tablet-2 ".to_string(),
                    "phone".to_string()
                ],
                true,
                &mut warnings
            )
            .unwrap(),
            vec!["phone".to_string(), "tablet-2".to_string()],
            "names are trimmed and de-duplicated"
        );
        assert!(warnings.is_empty());

        // Test time is strict: the operator sees the problem in the connection
        // test rather than in a failed notification.
        let error = validated_devices(&["my phone".to_string()], true, &mut warnings)
            .expect_err("a space is not in [A-Za-z0-9_-]");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("devices"));

        // A live send keeps delivering and lets Pushover decide.
        let mut warnings = Vec::new();
        let devices = validated_devices(&["x".repeat(26)], false, &mut warnings).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("devices"));
    }

    // -----------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------

    #[test]
    fn the_sparse_shape_the_core_sends_today_renders_a_title_and_a_message() {
        let (params, warnings) = params_of(&request(NotificationEventType::Grab), &settings());
        assert_eq!(params["title"], "Grabbed: Example Show");
        assert_eq!(
            params["message"],
            "Grabbed 'Example.Show.S01E01' for 'Example Show'."
        );
        assert_eq!(params["priority"], "0");
        assert_eq!(params["token"], "apitoken");
        assert_eq!(params["user"], "userkey");
        // Sonarr sends an empty `device`; an omitted one means the same thing
        // and keeps the form honest.
        assert!(!params.contains_key("device"));
        assert!(!params.contains_key("retry"));
        assert!(!params.contains_key("expire"));
        assert!(!params.contains_key("ttl"));
        assert!(!params.contains_key("sound"));
        assert!(!params.contains_key("encrypted"));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_grab_renders_the_release_facts_the_contract_carries() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.episode = Some(PluginNotificationEpisode {
            display: Some("1x01 - Pilot".to_string()),
            ..PluginNotificationEpisode::default()
        });
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Example.Show.S01E01.1080p.WEB-DL".to_string()),
            quality: Some("WEBDL-1080p".to_string()),
            release_group: Some("GROUP".to_string()),
            indexer: Some("Example Indexer".to_string()),
            ..PluginNotificationRelease::default()
        });
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            size_bytes: Some(2_147_483_648),
            ..PluginNotificationDownload::default()
        });

        let (params, _) = params_of(&req, &settings());
        let message = &params["message"];
        assert!(message.contains("Episode: 1x01 - Pilot"), "{message}");
        assert!(message.contains("Quality: WEBDL-1080p"), "{message}");
        assert!(
            message.contains("Release: Example.Show.S01E01.1080p.WEB-DL"),
            "{message}"
        );
        assert!(message.contains("Release Group: GROUP"), "{message}");
        assert!(message.contains("Indexer: Example Indexer"), "{message}");
        assert!(message.contains("Size: 2 GB"), "{message}");
        assert!(message.contains("Client: Weaver"), "{message}");
    }

    #[test]
    fn a_download_event_is_rendered_as_the_failure_it_is() {
        // The dispatcher maps DownloadFailed onto NotificationEventType::Download.
        let mut req = request(NotificationEventType::Download);
        req.summary_title = "Download failed: Example Show".to_string();
        req.summary_message = "The download client reported a failure.".to_string();
        req.severity = Some(NotificationSeverity::Error);
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            status: Some("failed".to_string()),
            ..PluginNotificationDownload::default()
        });

        let (params, _) = params_of(&req, &settings());
        assert_eq!(params["title"], "Download failed: Example Show");
        assert!(params["message"].contains("Status: failed"), "{params:?}");
        assert!(
            !params["message"].contains("Destination"),
            "a failed download has no import path"
        );
    }

    #[test]
    fn every_event_type_renders_without_a_panic_and_never_sends_an_empty_message() {
        for event_type in general_notification_events() {
            let mut req = request(event_type);
            req.summary_message = String::new();
            req.is_test = event_type == NotificationEventType::Test;
            let (params, _) = params_of(&req, &settings());
            assert!(
                !params["message"].is_empty(),
                "{event_type:?} produced an empty message"
            );
            assert!(
                !params["title"].is_empty(),
                "{event_type:?} produced an empty title"
            );
        }
    }

    #[test]
    fn a_deleted_file_and_a_health_issue_render_their_own_blocks() {
        let mut deleted = request(NotificationEventType::FileDeleted);
        deleted.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Example Show/S01E01.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });
        let (params, _) = params_of(&deleted, &settings());
        assert!(
            params["message"].contains("File: /media/TV/Example Show/S01E01.mkv"),
            "{params:?}"
        );

        let mut health = request(NotificationEventType::HealthIssue);
        health.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable".to_string()),
            ..PluginNotificationHealth::default()
        });
        let (params, _) = params_of(&health, &settings());
        assert!(params["message"].contains("Check: IndexerStatusCheck"));
        assert!(params["message"].contains("Detail: Indexers unavailable"));

        let mut update = request(NotificationEventType::ApplicationUpdate);
        update.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            ..PluginNotificationApplicationUpdate::default()
        });
        let (params, _) = params_of(&update, &settings());
        assert!(params["message"].contains("Previous Version: 0.19.7"));
        assert!(params["message"].contains("New Version: 0.19.8"));
    }

    #[test]
    fn an_import_renders_the_destination_and_a_rejection_renders_the_source() {
        let mut import = request(NotificationEventType::ImportComplete);
        import.import = Some(PluginNotificationImport {
            dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            source_title: Some("Example.Show.S01E01".to_string()),
            ..PluginNotificationImport::default()
        });
        let (params, _) = params_of(&import, &settings());
        assert!(
            params["message"].contains("Destination: /media/TV/Example Show/S01E01.mkv"),
            "{params:?}"
        );

        let mut rejected = request(NotificationEventType::ImportRejected);
        rejected.import = Some(PluginNotificationImport {
            source_path: Some("/downloads/Example.Show.S01E01".to_string()),
            status: Some("rejected".to_string()),
            ..PluginNotificationImport::default()
        });
        let (params, _) = params_of(&rejected, &settings());
        assert!(params["message"].contains("Source: /downloads/Example.Show.S01E01"));
        assert!(params["message"].contains("Status: rejected"));
    }

    #[test]
    fn subtitle_and_media_request_events_render_their_blocks() {
        let mut subtitles = request(NotificationEventType::SubtitleDownloaded);
        subtitles.media_files = vec![PluginNotificationMediaFile {
            path: "/media/TV/Example Show/S01E01.mkv".to_string(),
            subtitle_languages: vec!["English".to_string(), "German".to_string()],
            ..PluginNotificationMediaFile::default()
        }];
        let (params, _) = params_of(&subtitles, &settings());
        assert!(params["message"].contains("Languages: English, German"));
    }

    #[test]
    fn an_episode_display_is_composed_when_the_core_did_not_render_one() {
        let mut req = request(NotificationEventType::ImportComplete);
        req.episodes = vec![
            PluginNotificationEpisode {
                season_number: Some("1".to_string()),
                episode_number: Some("1".to_string()),
                title: Some("Pilot".to_string()),
                ..PluginNotificationEpisode::default()
            },
            PluginNotificationEpisode {
                season_number: Some("1".to_string()),
                episode_number: Some("2".to_string()),
                title: Some("Second".to_string()),
                ..PluginNotificationEpisode::default()
            },
        ];
        assert_eq!(
            episode_display(&req).as_deref(),
            Some("1x01x02 - Pilot + Second")
        );

        let mut daily = request(NotificationEventType::Grab);
        daily.episode = Some(PluginNotificationEpisode {
            air_date: Some("2026-09-01".to_string()),
            title: Some("Monday".to_string()),
            ..PluginNotificationEpisode::default()
        });
        assert_eq!(
            episode_display(&daily).as_deref(),
            Some("2026-09-01 - Monday")
        );
    }

    // -----------------------------------------------------------------
    // Limits (M2)
    // -----------------------------------------------------------------

    #[test]
    fn a_long_message_is_trimmed_to_1024_characters_with_a_warning() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_message = "y".repeat(4000);

        let (params, warnings) = params_of(&req, &settings());
        assert_eq!(char_count(&params["message"]), MESSAGE_CHARACTER_LIMIT);
        assert!(params["message"].ends_with('…'));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("1024"));
    }

    #[test]
    fn a_long_title_is_trimmed_to_250_characters_with_a_warning() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_title = "T".repeat(400);

        let (params, warnings) = params_of(&req, &settings());
        assert_eq!(char_count(&params["title"]), TITLE_CHARACTER_LIMIT);
        assert!(warnings.iter().any(|warning| warning.contains("title")));
    }

    #[test]
    fn enrichment_lines_are_dropped_before_the_summary_is_lost() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_message = "s".repeat(1000);
        req.release = Some(PluginNotificationRelease {
            quality: Some("WEBDL-1080p".to_string()),
            indexer: Some("Example Indexer".to_string()),
            ..PluginNotificationRelease::default()
        });

        let (params, warnings) = params_of(&req, &settings());
        assert!(params["message"].starts_with(&"s".repeat(1000)));
        assert!(char_count(&params["message"]) <= MESSAGE_CHARACTER_LIMIT);
        assert!(
            warnings.iter().any(|warning| warning.contains("dropped")),
            "{warnings:?}"
        );
    }

    // -----------------------------------------------------------------
    // Supplementary URL (M2)
    // -----------------------------------------------------------------

    #[test]
    fn auto_picks_the_facet_appropriate_metadata_link() {
        let mut series = request(NotificationEventType::Grab);
        series.title = Some(series_title());
        let (params, _) = params_of(&series, &settings());
        assert_eq!(params["url"], "https://thetvdb.com/?tab=series&id=12345");
        assert_eq!(params["url_title"], "Example Show on TVDb");

        let mut movie = request(NotificationEventType::Grab);
        movie.title = Some(movie_title());
        let (params, _) = params_of(&movie, &settings());
        assert_eq!(params["url"], "https://www.themoviedb.org/movie/603");
        assert_eq!(params["url_title"], "Example Movie on TMDb");
    }

    #[test]
    fn a_chosen_site_wins_and_none_suppresses_the_link() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let mut chosen = settings();
        chosen.metadata_link = "imdb".to_string();
        let (params, _) = params_of(&req, &chosen);
        assert_eq!(params["url"], "https://www.imdb.com/title/tt0903747");

        // A site the title has no id for renders nothing rather than a dead URL.
        let mut missing = settings();
        missing.metadata_link = "anidb".to_string();
        let (params, _) = params_of(&req, &missing);
        assert!(!params.contains_key("url"));

        let mut none = settings();
        none.metadata_link = METADATA_LINK_NONE.to_string();
        let (params, _) = params_of(&req, &none);
        assert!(!params.contains_key("url"));
    }

    #[test]
    fn a_manual_interaction_link_wins_over_the_metadata_link() {
        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.title = Some(series_title());
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            reason: Some("Import needs a decision".to_string()),
            link: Some("https://scryer.example/queue/1".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        let (params, _) = params_of(&req, &settings());
        assert_eq!(params["url"], "https://scryer.example/queue/1");
        assert_eq!(params["url_title"], "Open in Scryer");

        // A relative link is not a tap target.
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            link: Some("/queue/1".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        let (params, _) = params_of(&req, &settings());
        assert_eq!(params["url"], "https://thetvdb.com/?tab=series&id=12345");
    }

    #[test]
    fn a_test_notification_carries_a_working_link() {
        let (params, _) = params_of(&request(NotificationEventType::Test), &settings());
        assert_eq!(params["url"], SCRYER_LINK);
        assert_eq!(params["url_title"], "Scryer");
    }

    #[test]
    fn a_url_title_is_trimmed_to_100_characters() {
        let mut req = request(NotificationEventType::Grab);
        let mut title = series_title();
        title.name = "N".repeat(300);
        req.title = Some(title);
        let (params, _) = params_of(&req, &settings());
        assert_eq!(char_count(&params["url_title"]), URL_TITLE_CHARACTER_LIMIT);
    }

    // -----------------------------------------------------------------
    // Timestamp
    // -----------------------------------------------------------------

    #[test]
    fn occurred_at_becomes_the_pushover_timestamp() {
        let (params, _) = params_of(&request(NotificationEventType::Grab), &settings());
        assert_eq!(params["timestamp"], "1788264000");

        let mut undated = request(NotificationEventType::Grab);
        undated.occurred_at = None;
        let (params, _) = params_of(&undated, &settings());
        assert!(!params.contains_key("timestamp"));
    }

    #[test]
    fn rfc3339_instants_convert_to_unix_seconds() {
        assert_eq!(unix_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_timestamp("2026-09-01T12:00:00Z"), Some(1_788_264_000));
        assert_eq!(
            unix_timestamp("2026-09-01T12:00:00+00:00"),
            Some(1_788_264_000)
        );
        // The dispatcher's `to_rfc3339` emits nanoseconds and a signed offset.
        assert_eq!(
            unix_timestamp("2026-09-01T12:00:00.123456789+00:00"),
            Some(1_788_264_000)
        );
        assert_eq!(
            unix_timestamp("2026-09-01T14:30:00+02:30"),
            Some(1_788_264_000)
        );
        assert_eq!(
            unix_timestamp("2026-09-01T09:30:00-0230"),
            Some(1_788_264_000)
        );
        // A leap day is a real date.
        assert_eq!(unix_timestamp("2000-02-29T00:00:00Z"), Some(951_782_400));

        for bad in [
            "",
            "not a date",
            "2026-09-01",
            "2026-09-01T12:00:00",
            "2026-13-01T12:00:00Z",
            "2026-09-01T24:00:00Z",
            "2026-09-01T12:00:00.Z",
            "2026-09-01T12:00:00+2:00",
        ] {
            assert_eq!(unix_timestamp(bad), None, "{bad:?} must not parse");
        }
    }

    // -----------------------------------------------------------------
    // Emergency priority, ttl, sound, devices on the wire
    // -----------------------------------------------------------------

    #[test]
    fn emergency_priority_sends_retry_and_expire_and_nothing_else_does() {
        let mut emergency = settings();
        emergency.priority = 2;
        emergency.retry = 60;
        emergency.expire = 1800;
        let (params, _) = params_of(&request(NotificationEventType::Grab), &emergency);
        assert_eq!(params["priority"], "2");
        assert_eq!(params["retry"], "60");
        assert_eq!(params["expire"], "1800");

        let mut high = settings();
        high.priority = 1;
        high.retry = 60;
        high.expire = 1800;
        let (params, _) = params_of(&request(NotificationEventType::Grab), &high);
        assert_eq!(params["priority"], "1");
        assert!(!params.contains_key("retry"));
        assert!(!params.contains_key("expire"));
    }

    #[test]
    fn ttl_sound_and_devices_reach_the_form_when_set() {
        let mut configured = settings();
        configured.ttl = 3600;
        configured.sound = Some("cosmic".to_string());
        configured.devices = vec!["phone".to_string(), "tablet".to_string()];
        let (params, _) = params_of(&request(NotificationEventType::Grab), &configured);
        assert_eq!(params["ttl"], "3600");
        assert_eq!(params["sound"], "cosmic");
        assert_eq!(params["device"], "phone,tablet");
    }

    // -----------------------------------------------------------------
    // Encryption
    // -----------------------------------------------------------------

    fn decrypt_field(encoded: &str, key: &[u8; 32]) -> String {
        let payload = BASE64.decode(encoded).expect("base64");
        assert!(payload.len() > 48, "iv + ciphertext + mac");
        let (iv_and_ciphertext, signature) = payload.split_at(payload.len() - 32);
        let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
        mac.update(iv_and_ciphertext);
        mac.verify_slice(signature)
            .expect("HMAC-SHA256 must verify");

        let (iv, ciphertext) = iv_and_ciphertext.split_at(16);
        let iv: &[u8; 16] = iv.try_into().expect("a 16-byte IV");
        let compressed = cbc::Decryptor::<Aes256>::new(key.into(), iv.into())
            .decrypt_padded_vec::<Pkcs7>(ciphertext)
            .expect("AES-256-CBC/PKCS7");

        let mut plaintext = String::new();
        GzDecoder::new(compressed.as_slice())
            .read_to_string(&mut plaintext)
            .expect("gzip");
        plaintext
    }

    #[test]
    fn the_encrypted_wire_format_is_iv_ciphertext_hmac_over_gzipped_plaintext() {
        let key = parse_encryption_key(Some(TEST_KEY_HEX)).unwrap().unwrap();
        let iv = [0u8; 16];

        // A fixed IV pins the format byte for byte, which a randomly generated
        // one cannot.
        let encoded = encrypt_field_with_iv("Grabbed: Example Show", &key, &iv).unwrap();
        let payload = BASE64.decode(&encoded).unwrap();
        assert_eq!(&payload[..16], &iv, "the IV is the first 16 bytes");
        assert_eq!(
            (payload.len() - 16 - 32) % 16,
            0,
            "the ciphertext is a whole number of AES blocks"
        );
        assert_eq!(decrypt_field(&encoded, &key), "Grabbed: Example Show");

        // A different IV changes the ciphertext but not the plaintext.
        let other = encrypt_field_with_iv("Grabbed: Example Show", &key, &[9u8; 16]).unwrap();
        assert_ne!(other, encoded);
        assert_eq!(decrypt_field(&other, &key), "Grabbed: Example Show");
    }

    #[test]
    fn an_encrypted_send_encrypts_title_message_and_url_and_drops_url_title() {
        let key = parse_encryption_key(Some(TEST_KEY_HEX)).unwrap().unwrap();
        let mut encrypted = settings();
        encrypted.encryption_key = Some(key);

        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let (params, warnings) = params_of(&req, &encrypted);
        assert_eq!(params["encrypted"], "1");
        assert_eq!(
            decrypt_field(&params["title"], &key),
            "Grabbed: Example Show"
        );
        assert_eq!(
            decrypt_field(&params["message"], &key),
            "Grabbed 'Example.Show.S01E01' for 'Example Show'."
        );
        assert_eq!(
            decrypt_field(&params["url"], &key),
            "https://thetvdb.com/?tab=series&id=12345"
        );
        assert!(
            !params.contains_key("url_title"),
            "an encrypted url_title cannot fit 100 characters"
        );
        assert!(
            warnings.iter().any(|warning| warning.contains("url_title")),
            "{warnings:?}"
        );
        // The token and the user key are never encrypted.
        assert_eq!(params["token"], "apitoken");
        assert_eq!(params["user"], "userkey");
    }

    #[test]
    fn an_encrypted_message_is_shrunk_until_the_ciphertext_fits_the_limit() {
        let key = parse_encryption_key(Some(TEST_KEY_HEX)).unwrap().unwrap();
        let mut encrypted = settings();
        encrypted.encryption_key = Some(key);

        let mut req = request(NotificationEventType::Grab);
        // High-entropy multi-byte text does not compress, so 1024 plaintext
        // characters (~3 KiB) encrypt to far more than 1024 encoded characters
        // and the plaintext has to give way.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        req.summary_message = (0..2000)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                char::from_u32(0x4E00 + ((state >> 33) % 20_000) as u32).expect("a CJK codepoint")
            })
            .collect();

        let (params, warnings) = params_of(&req, &encrypted);
        assert!(char_count(&params["message"]) <= MESSAGE_CHARACTER_LIMIT);
        let plaintext = decrypt_field(&params["message"], &key);
        assert!(!plaintext.is_empty());
        assert!(char_count(&plaintext) < MESSAGE_CHARACTER_LIMIT);
        assert!(
            warnings.iter().any(|warning| warning.contains("encrypted")),
            "{warnings:?}"
        );
    }

    // -----------------------------------------------------------------
    // Response classification (H2)
    // -----------------------------------------------------------------

    fn classify(
        status: u16,
        header_pairs: &[(&str, &str)],
        body: &str,
    ) -> PluginResult<PluginNotificationResponse> {
        classify_response(status, &headers(header_pairs), body.as_bytes(), Vec::new())
    }

    #[test]
    fn a_success_reports_the_request_id_and_an_emergency_receipt() {
        let PluginResult::Ok(response) = classify(
            200,
            &[],
            r#"{"status":1,"request":"647d2300-702c-4b38-8b2f-d56326ae460b"}"#,
        ) else {
            panic!("a 200 is a delivery");
        };
        assert!(response.success);
        assert_eq!(
            response.delivery_id.as_deref(),
            Some("647d2300-702c-4b38-8b2f-d56326ae460b")
        );

        let PluginResult::Ok(response) = classify(
            200,
            &[],
            r#"{"status":1,"request":"req-1","receipt":"rMuUvNbmuBs3zpKZ9uMkPfxfXNYPFa"}"#,
        ) else {
            panic!("a 200 is a delivery");
        };
        assert_eq!(
            response.delivery_id.as_deref(),
            Some("rMuUvNbmuBs3zpKZ9uMkPfxfXNYPFa"),
            "the receipt is what polls or cancels an emergency alert"
        );
    }

    #[test]
    fn an_invalid_token_is_authfailed_on_api_key() {
        let PluginResult::Err(error) = classify(
            400,
            &[],
            r#"{"token":"invalid","errors":["application token is invalid"],"status":0,"request":"r"}"#,
        ) else {
            panic!("a 4xx is permanent, not a delivery");
        };
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("api_key"), "{error:?}");
        assert!(
            error
                .public_message
                .contains("application token is invalid")
        );
    }

    #[test]
    fn an_invalid_user_key_is_invalidconfig_on_user_key() {
        let PluginResult::Err(error) = classify(
            400,
            &[],
            r#"{"user":"invalid","errors":["user identifier is invalid"],"status":0,"request":"5042853c"}"#,
        ) else {
            panic!("a 4xx is permanent, not a delivery");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("user_key"), "{error:?}");
    }

    #[test]
    fn a_rejected_device_sound_or_emergency_parameter_names_its_setting() {
        for (body, setting) in [
            (
                r#"{"device":"invalid","errors":["device name is not valid"],"status":0}"#,
                "devices",
            ),
            (r#"{"errors":["sound is invalid"],"status":0}"#, "sound"),
            (
                r#"{"errors":["retry parameter must be at least 30 seconds"],"status":0}"#,
                "retry",
            ),
            (
                r#"{"errors":["expire parameter must be at most 10800 seconds"],"status":0}"#,
                "expire",
            ),
        ] {
            let PluginResult::Err(error) = classify(400, &[], body) else {
                panic!("a 4xx is permanent: {body}");
            };
            assert_eq!(error.code, PluginErrorCode::InvalidConfig, "{body}");
            assert!(
                error.public_message.starts_with(setting),
                "{setting} should be named: {}",
                error.public_message
            );
        }
    }

    #[test]
    fn a_message_pushover_refuses_is_this_plugins_bug() {
        let PluginResult::Err(error) = classify(
            400,
            &[],
            r#"{"errors":["message cannot be blank"],"status":0}"#,
        ) else {
            panic!("a 4xx is permanent");
        };
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("this plugin built"));
    }

    #[test]
    fn an_unattributed_4xx_still_reaches_the_operator_with_pushovers_own_words() {
        let PluginResult::Err(error) = classify(403, &[], "you are blocked") else {
            panic!("a 4xx is permanent");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(
            error.public_message.contains("you are blocked"),
            "{error:?}"
        );
        assert_eq!(
            error.debug_message.as_deref(),
            Some("HTTP 403: you are blocked")
        );
    }

    #[test]
    fn a_429_is_a_delivery_failure_carrying_the_quota_reset() {
        let reset = now_unix().unwrap() + 3_600;
        let PluginResult::Ok(response) = classify(
            429,
            &[
                ("X-Limit-App-Limit", "10000"),
                ("X-Limit-App-Remaining", "0"),
                ("X-Limit-App-Reset", &reset.to_string()),
            ],
            r#"{"status":0,"errors":["message limit reached"],"request":"r"}"#,
        ) else {
            panic!("a 429 is the provider saying no, not a broken configuration");
        };
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));
        let retry_after = response.retry_after_seconds.expect("retry_after_seconds");
        assert!(
            (3_500..=3_600).contains(&retry_after),
            "X-Limit-App-Reset is an absolute timestamp: {retry_after}"
        );
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("0 of 10000")),
            "{:?}",
            response.warnings
        );
    }

    #[test]
    fn a_retry_after_header_wins_over_the_absolute_reset() {
        assert_eq!(
            quota_reset_seconds(&headers(&[("Retry-After", "42")])),
            Some(42)
        );
        assert_eq!(quota_reset_seconds(&headers(&[])), None);
    }

    #[test]
    fn a_low_remaining_quota_warns_on_a_successful_send() {
        // 5% of the account's monthly pool, which since 1 May 2026 is shared by
        // every application on the account.
        assert!(
            quota_warning(&headers(&[
                ("X-Limit-App-Limit", "10000"),
                ("X-Limit-App-Remaining", "400"),
            ]))
            .is_some()
        );
        assert!(
            quota_warning(&headers(&[
                ("X-Limit-App-Limit", "10000"),
                ("X-Limit-App-Remaining", "9000"),
            ]))
            .is_none()
        );
        // No limit header: fall back to an absolute floor.
        assert!(quota_warning(&headers(&[("X-Limit-App-Remaining", "12")])).is_some());
        assert!(quota_warning(&headers(&[("X-Limit-App-Remaining", "5000")])).is_none());
        assert!(quota_warning(&headers(&[])).is_none());
    }

    #[test]
    fn a_5xx_is_a_retryable_delivery_failure() {
        let PluginResult::Ok(response) = classify(503, &[], "upstream exploded") else {
            panic!("a 5xx is the provider saying no right now");
        };
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_503"));
        assert_eq!(
            response.retry_after_seconds,
            Some(SERVER_ERROR_RETRY_SECONDS),
            "Pushover documents a five-second floor before repeating a request"
        );
    }

    #[test]
    fn a_200_that_says_status_zero_is_not_a_delivery() {
        let PluginResult::Err(error) = classify(
            200,
            &[],
            r#"{"status":0,"errors":["user identifier is invalid"],"user":"invalid"}"#,
        ) else {
            panic!("status 0 is a failure whatever the HTTP code says");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn warnings_survive_a_successful_delivery() {
        let result = classify_response(
            200,
            &headers(&[]),
            br#"{"status":1,"request":"r"}"#,
            vec!["message trimmed".to_string()],
        );
        let PluginResult::Ok(response) = result else {
            panic!("a 200 is a delivery");
        };
        assert_eq!(response.warnings, vec!["message trimmed".to_string()]);
    }
}
