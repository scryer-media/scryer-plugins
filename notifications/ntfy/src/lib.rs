//! ntfy push notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's ntfy notification (`src/NzbDrone.Core/Notifications/Ntfy/`) is a
//! thin channel: it loops over the configured topics, `POST`s an empty body to
//! `{server}/{topic}` and puts `title`, `message`, `priority`, `tags` and
//! `click` in the **query string** (`NtfyProxy.cs:117-128`). Auth is a Bearer
//! token or Basic credentials, and the operator's extra headers ride along
//! (`NtfyProxy.cs:131-143`). Its one piece of hard-won knowledge is the failure
//! attribution in `Test`: a 401/403 is the access token, or the username and
//! password, or "authorization is required" when neither is configured, and
//! anything else is the server URL (`NtfyProxy.cs:83-98`).
//!
//! The June port copied that shape verbatim and then reported its *own*
//! configuration checks — an empty topic list, a topic that is not a legal ntfy
//! topic, a priority outside 1-5, a half-configured username/password pair — as
//! **delivery failures** (`error_response`), which tells the operator "a
//! notification failed to send" when the truth is "this channel is misconfigured
//! and no notification was ever attempted".
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * **JSON publishing** (`POST {server}/` with a `topic` field) replaces the
//!   query-string publish. Same wire semantics, but the message no longer has to
//!   survive URL encoding or fit in a URL, and it unlocks `icon` for the title's
//!   poster. The stored settings are unchanged;
//! * **per-topic `target_results`**: one entry per topic with its own status and
//!   error, instead of merging every response into a bag of warnings. `success`
//!   is true only when every topic was accepted. This matters for ntfy in
//!   particular, because an access token can be granted write on one topic and
//!   refused on another;
//! * **typed errors that name the field**: `topics`, `priority`,
//!   `failure_priority`, `access_token`, `username`, `password`, `server_url`,
//!   `click_url`, `headers`, `metadata_links`, `preferred_metadata_link`. ntfy's
//!   own error body (`{code, http, error, link}`) is parsed and its `code` is
//!   used to attribute a 400 to the setting that caused it;
//! * **field types that match the setting**: `topics` and `tags` are `Tag`
//!   fields and `priority` is a `Select` over ntfy's Min/Low/Default/High/Max,
//!   which is what Sonarr renders (`NtfySettings.cs:50-57`) and what the June
//!   port lost;
//! * the body is **enriched per event** from the structured blocks the contract
//!   carries (episode, quality, release, indexer, client, size, paths, health,
//!   version) rather than being `summary_message` alone, and the metadata links
//!   the sibling channels render are available here too, with one of them
//!   selectable as ntfy's `click` target;
//! * ntfy's documented limits are **respected before the send**: a message over
//!   4096 bytes is silently turned into a file attachment by the server and a
//!   title over 1024 bytes is rejected outright, so both are truncated with a
//!   `warnings` entry instead.
//!
//! # Why the delivery path is local rather than `notify_common::send_bytes`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")` and has no notion of a per-target result. ntfy's failures
//! are four different lanes in Scryer's contract: a 401/403 is a credential
//! setting (which one depends on what is configured), a 404 or a body that is
//! not ntfy's error JSON is the `server_url`, a 400 is either the message this
//! plugin built or a setting ntfy's error code names, and only a 429/507/5xx is
//! the provider saying "not now".
//!
//! # Upstream reference
//!
//! Read 2026-09-02:
//!
//! * <https://docs.ntfy.sh/publish/> — "Publish as JSON": `POST` to the server
//!   root with a JSON body whose fields are `topic` (required), `message`,
//!   `title`, `tags`, `priority`, `click`, `actions`, `attach`, `markdown`,
//!   `icon`, `filename`, `delay`, `email`, `call`, `cache`, `firebase`. The
//!   same page documents the priority names (1 `min`, 2 `low`, 3 `default`,
//!   4 `high`, 5 `max`/`urgent`), that a tag matching an emoji short code is
//!   converted and prepended, that `icon` accepts only JPEG and PNG, that the
//!   maximum message length is 4096 bytes and a longer message "will
//!   automatically […] send the message as an attachment file", and that
//!   non-ASCII headers need RFC 2047 encoding — a hazard JSON publishing does
//!   not have.
//! * <https://docs.ntfy.sh/publish/#authentication> — `Authorization: Bearer
//!   tk_…` for access tokens, `Authorization: Basic base64(user:pass)` for
//!   username/password, 401 for missing or invalid credentials and 403 for
//!   valid credentials without permission on the topic.
//! * <https://github.com/binwiederhier/ntfy/blob/main/server/errors.go> — the
//!   error body `{"code":…,"http":…,"error":…,"link":…}` and the codes this
//!   module attributes: `40007` invalid priority, `40009` topic invalid,
//!   `40010` topic name is not allowed, `40024` body must be valid JSON,
//!   `40101` unauthorized, `40301` forbidden, `41303` JSON body too large,
//!   `42901`/`42902`/`42903`/`42909` rate limits, `50701` UnifiedPush topic
//!   without a subscriber.
//! * <https://github.com/binwiederhier/ntfy/blob/main/server/server.go> —
//!   `topicRegex = ^[-_A-Za-z0-9]{1,64}$`, `apiHealthPath = "/v1/health"`, and
//!   the JSON publish route requiring `m.Topic` to match that regex.
//! * <https://github.com/binwiederhier/ntfy/blob/main/server/config.go> —
//!   `DefaultMessageSizeLimit` 4096, `messageTitleSizeLimit` 1024,
//!   `messageTagsSizeLimit` 512, and `DefaultDisallowedTopics` = `docs`,
//!   `static`, `file`, `app`, `metrics`, `account`, `settings`, `signup`,
//!   `login`, `v1`.
//! * <https://docs.ntfy.sh/releases/> — ntfy server v2.28.0 (2026-08-27) is the
//!   current release; markdown rendering in the web app arrived in v2.7.0
//!   (2023-08-17). Nothing this channel uses is deprecated.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationSeverity,
    PluginNotificationEpisode, PluginNotificationTargetResult, current_sdk_constraint,
};
use serde_json::{Map, Value, json};

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

const PROVIDER_TYPE: &str = "ntfy";
const USER_AGENT: &str = concat!("scryer-ntfy-plugin/", env!("CARGO_PKG_VERSION"));

/// `NtfyProxy.cs:22` — the public instance, used when `server_url` is empty.
const DEFAULT_SERVER_URL: &str = "https://ntfy.sh";
const DEFAULT_SERVER_HOST: &str = "ntfy.sh";

/// The link shown on a test message. Sonarr's ntfy channel puts no link on its
/// test (`NtfyProxy.cs:71-78`); this one does, so the operator can confirm
/// that the `click` target survived the trip.
const SCRYER_LINK: &str = "https://github.com/scryer-media/scryer";

/// `apiHealthPath` in `server/server.go`. Unauthenticated, and answers
/// `{"healthy":true}`.
const HEALTH_PATH: &str = "/v1/health";

/// `DefaultMessageSizeLimit` (`server/config.go`). A longer message is not
/// rejected: the server "will automatically detect the mime type and size, and
/// send the message as an attachment file" (docs.ntfy.sh/publish), which turns a
/// notification into a file nobody reads. Truncating is strictly better.
const MAX_MESSAGE_BYTES: usize = 4096;

/// `messageTitleSizeLimit` (`server/config.go`). Unlike the message, an
/// over-long title is a hard `errHTTPBadRequestTitleTooLarge`.
const MAX_TITLE_BYTES: usize = 1024;

/// `messageTagsSizeLimit` (`server/config.go`), measured over the joined tag
/// list.
const MAX_TAGS_BYTES: usize = 512;

/// The one line of upstream error text quoted back to the operator. A server
/// that answers with a whole HTML page must not turn a failed notification into
/// a wall of markup.
const MAX_QUOTED_ERROR: usize = 300;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `NtfyPriority.cs`, annotated with what ntfy's clients actually do with each
/// value (<https://docs.ntfy.sh/publish/#message-priority>). Sonarr renders
/// these as a select (`NtfySettings.cs:50-51`); the June port made the field a
/// free `Number`, so an operator could store `urgent` and have it silently
/// delivered as `3`. The stored values are Sonarr's integers, unchanged.
const PRIORITY_OPTIONS: &[(&str, &str)] = &[
    ("1", "Min (1) - no sound or vibration"),
    (
        "2",
        "Low (2) - no sound or vibration, hidden until the drawer is pulled",
    ),
    ("3", "Default (3) - short vibration and the default sound"),
    ("4", "High (4) - long vibration burst and a pop-over"),
    ("5", "Max (5) - very long vibration bursts and a pop-over"),
];

/// ntfy also accepts the priority by name. An operator who typed one into the
/// old free-text `Number` field stored a string `config_i64` silently threw
/// away; parsing it here keeps that configuration working and makes the value
/// mean what it says.
const PRIORITY_NAMES: &[(&str, i64)] = &[
    ("min", 1),
    ("low", 2),
    ("default", 3),
    ("high", 4),
    ("max", 5),
    ("urgent", 5),
];

const MIN_PRIORITY: i64 = 1;
const MAX_PRIORITY: i64 = 5;

/// "use `priority`" — the default, and Sonarr's only behaviour.
const FAILURE_PRIORITY_SAME: &str = "same";

/// The priority used when the event's severity is `Warning` or `Error`.
///
/// Sonarr sends one priority for every event, so a failed download buzzes
/// exactly as loudly as a rename. Scryer's dispatcher stamps a severity on every
/// notification (`dispatcher.rs:895`, `:920-928`), which is enough to make
/// failures louder — but only when the operator asks for it, because overriding
/// a deliberate `Min (1)` would un-mute exactly the channel they muted.
fn failure_priority_options() -> Vec<(&'static str, &'static str)> {
    let mut options = vec![(FAILURE_PRIORITY_SAME, "Same as Priority")];
    options.extend_from_slice(PRIORITY_OPTIONS);
    options
}

/// `topicRegex` (`server/server.go`) plus `DefaultDisallowedTopics`
/// (`server/config.go`).
///
/// Sonarr's list (`NtfySettings.cs:23`) is the one ntfy's documentation carried
/// in 2022: `announcements`, `app`, `docs`, `settings`, `stats`, `mytopic-rw`,
/// `mytopic-ro`, `mytopic-wo`. Three of those (`mytopic-*`) were only ever
/// examples in the access-control documentation and `announcements`/`stats` are
/// not disallowed by any ntfy server, so refusing them would block a
/// configuration that works today. The current default list is used instead;
/// `docs`, `app` and `settings` appear in both.
const DISALLOWED_TOPICS: &[&str] = &[
    "docs", "static", "file", "app", "metrics", "account", "settings", "signup", "login", "v1",
];

const MAX_TOPIC_LENGTH: usize = 64;

/// The sibling channels' metadata-link set, generalised from Sonarr's
/// series-only world to Scryer's facets. Sonarr's ntfy channel has no such
/// setting; its `click` is a fixed URL (`NtfySettings.cs:59-60`), which is kept
/// as `click_url` and remains the fallback.
const METADATA_LINK_OPTIONS: &[(&str, &str)] = &[
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

/// The default: keep Sonarr's behaviour, where `click` is whatever the operator
/// typed into `click_url` and nothing else.
const PREFERRED_LINK_NONE: &str = "none";

fn preferred_link_options() -> Vec<(&'static str, &'static str)> {
    let mut options = vec![(PREFERRED_LINK_NONE, "None (use Click URL)")];
    options.extend_from_slice(METADATA_LINK_OPTIONS);
    options
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "ntfy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            // Sonarr calls this channel "ntfy.sh" (`Ntfy.cs:16`). The core does
            // not consume aliases today, so this is documentation for an
            // importer rather than behaviour.
            provider_aliases: vec!["ntfy.sh".to_string()],
            default_base_url: Some(DEFAULT_SERVER_URL.to_string()),
            // The public instance, for the default configuration. A self-hosted
            // `server_url` is allowlisted by the loader, which adds the host of
            // every config value that parses as a URL
            // (`crates/scryer-plugins/src/loader.rs:3142-3151`).
            allowed_hosts: vec![DEFAULT_SERVER_HOST.to_string()],
            capabilities: NotificationCapabilities {
                // Deliberately false: see the `markdown` note in `build_payload`.
                supports_rich_text: false,
                // The poster travels as a URL in `icon`. No bytes are uploaded.
                supports_images: true,
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
        connection_field(
            "server_url",
            "Server URL",
            false,
            Some(DEFAULT_SERVER_URL),
            Some("ntfy server URL; defaults to https://ntfy.sh."),
        ),
        field(
            "access_token",
            "Access Token",
            ConfigFieldType::Password,
            false,
            None,
            Some("An ntfy access token (tk_…). Takes precedence over the username and password."),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some("HTTP Basic username. Provide both a username and a password, or neither."),
        ),
        field(
            "password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            Some("HTTP Basic password. Provide both a username and a password, or neither."),
        ),
        select_field("priority", "Priority", Some("3"), PRIORITY_OPTIONS),
        select_field(
            "failure_priority",
            "Failure Priority",
            Some(FAILURE_PRIORITY_SAME),
            &failure_priority_options(),
        ),
        tag_field(
            "topics",
            "Topics",
            true,
            &[],
            Some(
                "ntfy topics to publish to. Letters, numbers, underscores and dashes, up to 64 characters.",
            ),
        ),
        tag_field(
            "tags",
            "Tags",
            false,
            &[],
            Some(
                "ntfy tags. A tag that matches an emoji short code (warning, skull, +1) becomes an emoji on the notification; anything else is listed below it.",
            ),
        ),
        connection_field(
            "click_url",
            "Click URL",
            false,
            None,
            Some(
                "Opened when the notification is tapped. Used unless Preferred Metadata Link resolves to a link for this title.",
            ),
        ),
        field(
            "include_app_name_in_title",
            "Include App Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Prefixes the notification title with the Scryer application name."),
        ),
        field(
            "include_poster",
            "Include Poster",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Sends the title's poster as ntfy's notification icon. ntfy renders only JPEG and PNG images.",
            ),
        ),
        tag_field(
            "metadata_links",
            "Metadata Links",
            false,
            METADATA_LINK_OPTIONS,
            Some(
                "Metadata sites to link at the end of the message. Only the sites the title actually has an id for are rendered.",
            ),
        ),
        select_field(
            "preferred_metadata_link",
            "Preferred Metadata Link",
            Some(PREFERRED_LINK_NONE),
            &preferred_link_options(),
        ),
        field(
            "headers",
            "Headers",
            ConfigFieldType::Multiline,
            false,
            None,
            Some("Additional headers, one per line as Header-Name: value."),
        ),
    ]
}

/// A multi-value field, optionally with a fixed option set.
///
/// Scryer's notification settings UI renders a `Tag` field as a plain
/// comma-separated text input (`settings-notifications-section.tsx:242-345`
/// handles `BOOL`, `SELECT` and `MULTILINE` and falls through to a text input
/// for everything else), so this is the same box the operator already types
/// into and the stored value is unchanged — which is what keeps existing
/// `topics`/`tags` configurations parsing.
fn tag_field(
    key: &str,
    label: &str,
    required: bool,
    options: &[(&str, &str)],
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        options: options
            .iter()
            .map(|(value, label)| ConfigFieldOption {
                value: (*value).to_string(),
                label: (*label).to_string(),
                config_overrides: Default::default(),
            })
            .collect(),
        ..field(key, label, ConfigFieldType::Tag, required, None, help_text)
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// How the request authenticates, resolved once so the send path cannot
/// disagree with the error attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Auth {
    None,
    /// `Authorization: Bearer tk_…`.
    Token(String),
    /// `Authorization: Basic base64(user:pass)`.
    Basic(String, String),
}

impl Auth {
    fn header(&self) -> Option<String> {
        match self {
            Auth::None => None,
            Auth::Token(token) => Some(format!("Bearer {token}")),
            Auth::Basic(username, password) => Some(basic_auth_header(username, password)),
        }
    }

    /// Sonarr's exact 401/403 split (`NtfyProxy.cs:83-98`): the token, or the
    /// username, or "authorization is required" when neither is configured.
    fn rejected_field(&self) -> (&'static str, &'static str) {
        match self {
            Auth::Token(_) => (
                "access_token",
                "the configured access token was rejected by ntfy",
            ),
            Auth::Basic(..) => (
                "username",
                "the configured username or password was rejected by ntfy",
            ),
            Auth::None => (
                "access_token",
                "this topic requires authorization, but no access token and no username and password are configured",
            ),
        }
    }
}

/// Everything the renderer and the sender need from configuration, resolved and
/// validated once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    server: String,
    auth: Auth,
    priority: i64,
    /// `None` means "use `priority`".
    failure_priority: Option<i64>,
    topics: Vec<String>,
    tags: Vec<String>,
    click_url: Option<String>,
    include_app_name_in_title: bool,
    include_poster: bool,
    metadata_links: Vec<String>,
    preferred_metadata_link: String,
    headers: Vec<(String, String)>,
}

impl Settings {
    /// `strict` is the Test-time posture. Rules ntfy itself will enforce (an
    /// unusable server URL, an illegal topic, a priority outside 1-5, a
    /// half-configured credential pair) are errors on every send, because the
    /// notification cannot be delivered either way. The two rules that only cost
    /// decoration — a `preferred_metadata_link` naming an unselected site, and a
    /// malformed line in `headers` — are refused at Test time and degraded to a
    /// warning on a live send, because neither is worth losing a notification
    /// over.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let mut warnings = Vec::new();

        let server = normalized_server(
            config_value("server_url")
                .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
                .as_str(),
        )?;
        let auth = resolve_auth(
            config_value("access_token"),
            config_value("username"),
            config_value("password"),
            &mut warnings,
        )?;
        let priority =
            parse_priority("priority", config_value("priority").as_deref(), Some(3))?.unwrap_or(3);
        let failure_priority = parse_priority(
            "failure_priority",
            config_value("failure_priority").as_deref(),
            None,
        )?;
        let topics = validated_topics(&config_csv("topics"))?;
        let tags = config_csv("tags");
        let click_url = validated_click_url(config_value("click_url").as_deref())?;
        let metadata_links = validated_metadata_links(&config_csv("metadata_links"))?;
        let preferred_metadata_link = validated_preferred_link(
            config_value("preferred_metadata_link").as_deref(),
            &metadata_links,
            strict,
        )?;
        let headers = parse_headers(config_value("headers").as_deref(), strict, &mut warnings)?;

        Ok((
            Self {
                server,
                auth,
                priority,
                failure_priority,
                topics,
                tags,
                click_url,
                include_app_name_in_title: config_bool("include_app_name_in_title"),
                include_poster: config_bool("include_poster"),
                metadata_links,
                preferred_metadata_link,
                headers,
            },
            warnings,
        ))
    }
}

/// `NtfySettingsValidator` (`NtfySettings.cs:16`): `RuleFor(c =>
/// c.ServerUrl).IsValidUrl()`.
///
/// Sonarr can only say this through its settings form. Here it is a typed
/// `InvalidConfig` naming the field, because the alternative — the June port's
/// bare `trim_end_matches('/')` — turns `ntfy.example` into a request the host
/// refuses with a message about an unsupported scheme.
fn normalized_server(raw: &str) -> Result<String, PluginError> {
    let trimmed = raw.trim().trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "server_url must be an absolute http:// or https:// URL, for example https://ntfy.sh"
                .to_string(),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    if trimmed.len() <= lower.find("//").map(|at| at + 2).unwrap_or(0) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "server_url has no host, for example https://ntfy.sh".to_string(),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    Ok(trimmed.to_string())
}

/// `NtfySettings.cs:18-19`: a username without a password (or the reverse) is a
/// settings error, and only when no access token is configured.
///
/// The June port reported the same rule as a *delivery* failure, so the operator
/// was told a notification failed rather than that half a credential is stored.
fn resolve_auth(
    access_token: Option<String>,
    username: Option<String>,
    password: Option<String>,
    warnings: &mut Vec<String>,
) -> Result<Auth, PluginError> {
    if let Some(token) = access_token {
        // ntfy mints access tokens with a `tk_` prefix
        // (docs.ntfy.sh/publish/#access-tokens). A value without it is usually a
        // password pasted into the wrong box, but self-hosted deployments and an
        // auth proxy in front of ntfy can both make it legitimate, so this is a
        // warning rather than a refusal.
        if !token.starts_with("tk_") {
            warnings.push(
                "access_token does not look like an ntfy access token (they start with 'tk_'); if this is a password, configure username and password instead"
                    .to_string(),
            );
        }
        return Ok(Auth::Token(token));
    }
    match (username, password) {
        (Some(username), Some(password)) => Ok(Auth::Basic(username, password)),
        (Some(_), None) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "password is required when username is configured".to_string(),
            None,
        )),
        (None, Some(_)) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "username is required when password is configured".to_string(),
            None,
        )),
        (None, None) => Ok(Auth::None),
    }
}

/// `NtfySettings.cs:15`: `RuleFor(c => c.Priority).InclusiveBetween(1, 5)`.
///
/// `config_i64` — what the June port used — silently substitutes the default, so
/// `priority = "urgent"` became `3` and nobody was told. ntfy accepts the names
/// as well as the numbers, so they are parsed rather than rejected.
fn parse_priority(
    key: &'static str,
    raw: Option<&str>,
    default_value: Option<i64>,
) -> Result<Option<i64>, PluginError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(default_value);
    };
    if raw.eq_ignore_ascii_case(FAILURE_PRIORITY_SAME) {
        return Ok(None);
    }
    if let Some((_, value)) = PRIORITY_NAMES
        .iter()
        .find(|(name, _)| raw.eq_ignore_ascii_case(name))
    {
        return Ok(Some(*value));
    }
    let priority = raw.parse::<i64>().map_err(|error| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "{key} must be a number from {MIN_PRIORITY} to {MAX_PRIORITY}, or one of min, low, default, high, max, urgent; got {raw:?}"
            ),
            Some(error.to_string()),
        )
    })?;
    if !(MIN_PRIORITY..=MAX_PRIORITY).contains(&priority) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "{key} must be between {MIN_PRIORITY} and {MAX_PRIORITY}; got {priority}. ntfy maps 1 to no sound or vibration, 3 to the default, and 5 to a pop-over with long vibration bursts."
            ),
            None,
        ));
    }
    Ok(Some(priority))
}

/// `NtfySettings.cs:14,20`: the topic list may not be empty and every topic must
/// match `[a-zA-Z0-9_-]+` and not be a reserved name.
///
/// The June port reported both as delivery failures. They are settings, so they
/// are typed `InvalidConfig` errors naming `topics`. Topic names are
/// case-sensitive on ntfy's side, so the de-duplication here is too.
fn validated_topics(configured: &[String]) -> Result<Vec<String>, PluginError> {
    if configured.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "topics is not configured: at least one ntfy topic is required".to_string(),
            None,
        ));
    }

    let mut topics: Vec<String> = Vec::new();
    for topic in configured {
        if topic.len() > MAX_TOPIC_LENGTH
            || !topic.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "topics contains an invalid ntfy topic: {topic:?}. Topic names may only contain letters, numbers, underscores and dashes, and may be up to {MAX_TOPIC_LENGTH} characters long."
                ),
                None,
            ));
        }
        if DISALLOWED_TOPICS.contains(&topic.to_ascii_lowercase().as_str()) {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "topics contains a topic ntfy reserves for itself: {topic:?}. Reserved names are {}.",
                    DISALLOWED_TOPICS.join(", ")
                ),
                None,
            ));
        }
        if !topics.contains(topic) {
            topics.push(topic.clone());
        }
    }
    Ok(topics)
}

/// `NtfySettings.cs:17`: `RuleFor(c => c.ClickUrl).IsValidUrl()`.
///
/// ntfy's `click` is not restricted to http(s) — `mailto:`, `geo:`, `ntfy://`
/// and app schemes all work (docs.ntfy.sh/publish/#click-action) — so this
/// checks for a scheme rather than for HTTP.
fn validated_click_url(raw: Option<&str>) -> Result<Option<String>, PluginError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    if !has_uri_scheme(raw) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "click_url must be an absolute URL with a scheme, for example https://scryer.example or mailto:me@example.com; got {raw:?}"
            ),
            None,
        ));
    }
    Ok(Some(raw.to_string()))
}

/// A scheme followed by `:`, per RFC 3986: an ASCII letter then letters, digits,
/// `+`, `-` or `.`.
fn has_uri_scheme(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && scheme.starts_with(|character: char| character.is_ascii_alphabetic())
        && scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

fn validated_metadata_links(selected: &[String]) -> Result<Vec<String>, PluginError> {
    let mut links = Vec::new();
    for value in selected {
        let value = value.to_ascii_lowercase();
        if !METADATA_LINK_OPTIONS.iter().any(|(key, _)| *key == value) {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("metadata_links contains an unknown value: {value}"),
                Some(format!(
                    "known values: {}",
                    option_keys(METADATA_LINK_OPTIONS)
                )),
            ));
        }
        if !links.contains(&value) {
            links.push(value);
        }
    }
    Ok(links)
}

fn validated_preferred_link(
    raw: Option<&str>,
    metadata_links: &[String],
    strict: bool,
) -> Result<String, PluginError> {
    let value = raw
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| PREFERRED_LINK_NONE.to_string());

    let options = preferred_link_options();
    if !options.iter().any(|(key, _)| *key == value) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("preferred_metadata_link is not a valid value: {value}"),
            Some(format!("known values: {}", option_keys(&options))),
        ));
    }

    if strict && value != PREFERRED_LINK_NONE && !metadata_links.iter().any(|link| link == &value) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "preferred_metadata_link must be one of the selected metadata_links; {value} is not selected"
            ),
            Some(format!(
                "selected metadata_links: {}",
                metadata_links.join(", ")
            )),
        ));
    }

    Ok(value)
}

/// `NtfySettings.cs:62-63` is a `KeyValueList`; Scryer has no such field type,
/// so this stays a `Multiline` of `Header-Name: value` lines.
///
/// The June port leaked every header name with `Box::leak` to satisfy a
/// `&'static str` parameter — an unbounded leak driven by configuration, on a
/// component instance the host may keep alive across many sends. The names are
/// owned here instead.
fn parse_headers(
    raw: Option<&str>,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<(String, String)>, PluginError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut headers = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let name = line.split_once(':').map(|(name, _)| name.trim());
        let malformed = match name {
            None => Some("expected 'Header-Name: value'"),
            Some("") => Some("the header name is empty"),
            Some(name) if !is_header_name(name) => {
                Some("the header name contains characters HTTP does not allow")
            }
            Some(_) => None,
        };
        if let Some(reason) = malformed {
            let message =
                format!("headers has a line this channel cannot use ({reason}): {line:?}");
            if strict {
                return Err(plugin_error(PluginErrorCode::InvalidConfig, message, None));
            }
            warnings.push(format!("{message}; the line was skipped"));
            continue;
        }
        let (name, value) = line.split_once(':').expect("checked above");
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(headers)
}

/// RFC 9110 `token`: the characters a header field name may contain.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '.'
                        | '^'
                        | '_'
                        | '`'
                        | '|'
                        | '~'
                )
        })
}

fn option_keys(options: &[(&str, &str)]) -> String {
    options
        .iter()
        .map(|(key, _)| *key)
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// `Ntfy.cs:21-69` sends a fixed constant per event, some branded "Sonarr - …"
/// and some not. Scryer's dispatcher already composes an event-specific,
/// title-bearing heading in `summary_title` ("Grabbed: Example Show"), which is
/// strictly more informative in a push notification whose title is what the lock
/// screen shows — so that is used as-is, and Sonarr's branding is available as
/// an opt-in prefix.
fn heading(req: &PluginNotificationRequest, settings: &Settings) -> String {
    let app = req.app.name.trim();
    let title = req.summary_title.trim();
    let base = if title.is_empty() {
        if app.is_empty() { "Scryer" } else { app }
    } else {
        title
    };
    if settings.include_app_name_in_title && !app.is_empty() && base != app {
        format!("{app} - {base}")
    } else {
        base.to_string()
    }
}

/// One line of the notification body.
///
/// ntfy's default content type is plain text and this channel keeps it that way
/// (see `build_payload`), so there is no escaping stage: a release name with `*`
/// or `_` in it arrives exactly as it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Plain(String),
    Labeled(&'static str, String),
}

impl Line {
    fn render(&self) -> String {
        match self {
            Line::Plain(value) => value.clone(),
            Line::Labeled(label, value) => format!("{label}: {value}"),
        }
    }
}

fn build_lines(
    req: &PluginNotificationRequest,
    settings: &Settings,
    links: &[(String, &'static str, String)],
) -> Vec<Line> {
    let mut lines = Vec::new();

    let message = req.summary_message.trim();
    if !message.is_empty() {
        lines.push(Line::Plain(message.to_string()));
    }

    lines.extend(detail_lines(req));

    for (_, label, url) in links {
        lines.push(Line::Labeled(label, url.clone()));
    }

    if req.is_test {
        lines.push(Line::Labeled("Scryer", SCRYER_LINK.to_string()));
    }

    // ntfy substitutes the message with the topic name when `message` is empty,
    // so an event whose summary and blocks are all blank still gets its heading.
    if lines.is_empty() {
        lines.push(Line::Plain(heading(req, settings)));
    }

    lines
}

/// The structured enrichment Sonarr's ntfy channel has no room for: Sonarr hands
/// the proxy one prose sentence, while Scryer's contract carries the facts
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
        // verified on both release-0.19.8 and release-NEXT). A successful import
        // is `ImportComplete`/`Upgrade`, so this arm renders a failure and never
        // an import path.
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

/// Only an absolute http(s) link is offered as a click target: tapping the
/// notification opens it on the device, and a relative path is a dead tap.
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
// Metadata links
// ---------------------------------------------------------------------------

/// The facet decides what "Trakt" and "TMDb" mean, which is the part Sonarr's
/// series-only model cannot express. A selected site with no id renders nothing
/// rather than a dead URL.
fn selected_metadata_links(
    req: &PluginNotificationRequest,
    selected: &[String],
) -> Vec<(String, &'static str, String)> {
    let Some(title) = req.title.as_ref() else {
        return Vec::new();
    };
    let ids = &title.external_ids;
    let episodic = matches!(
        title.facet.to_ascii_lowercase().as_str(),
        "series" | "anime" | "tv" | "show"
    );

    let imdb = external_id(ids.imdb_id.as_deref(), ids, "imdb");
    let tvdb = external_id(ids.tvdb_id.as_deref(), ids, "tvdb");
    let tmdb = external_id(ids.tmdb_id.as_deref(), ids, "tmdb");
    let tvmaze = external_id(ids.tvmaze_id.as_deref(), ids, "tvmaze");
    let anidb = external_id(ids.anidb_id.as_deref(), ids, "anidb");
    let anilist = external_id(ids.anilist_ids.first().map(String::as_str), ids, "anilist");
    let mal = external_id(ids.mal_ids.first().map(String::as_str), ids, "mal");
    let kitsu = external_id(ids.kitsu_ids.first().map(String::as_str), ids, "kitsu");

    let mut links = Vec::new();
    for key in selected {
        let link = match key.as_str() {
            "imdb" => imdb
                .as_ref()
                .map(|id| ("IMDb", format!("https://www.imdb.com/title/{id}"))),
            "tvdb" => tvdb
                .as_ref()
                .map(|id| ("TVDb", format!("https://thetvdb.com/?tab=series&id={id}"))),
            "tvmaze" => tvmaze
                .as_ref()
                .map(|id| ("TVMaze", format!("https://www.tvmaze.com/shows/{id}"))),
            "trakt" => {
                if episodic {
                    tvdb.as_ref().map(|id| {
                        (
                            "Trakt",
                            format!("https://trakt.tv/search/tvdb/{id}?id_type=show"),
                        )
                    })
                } else {
                    tmdb.as_ref()
                        .map(|id| {
                            (
                                "Trakt",
                                format!("https://trakt.tv/search/tmdb/{id}?id_type=movie"),
                            )
                        })
                        .or_else(|| {
                            imdb.as_ref()
                                .map(|id| ("Trakt", format!("https://trakt.tv/search/imdb/{id}")))
                        })
                }
            }
            "tmdb" => tmdb.as_ref().map(|id| {
                (
                    "TMDb",
                    if episodic {
                        format!("https://www.themoviedb.org/tv/{id}")
                    } else {
                        format!("https://www.themoviedb.org/movie/{id}")
                    },
                )
            }),
            "anidb" => anidb
                .as_ref()
                .map(|id| ("AniDB", format!("https://anidb.net/anime/{id}"))),
            "anilist" => anilist
                .as_ref()
                .map(|id| ("AniList", format!("https://anilist.co/anime/{id}"))),
            "mal" => mal
                .as_ref()
                .map(|id| ("MyAnimeList", format!("https://myanimelist.net/anime/{id}"))),
            "kitsu" => kitsu
                .as_ref()
                .map(|id| ("Kitsu", format!("https://kitsu.app/anime/{id}"))),
            _ => None,
        };
        if let Some((label, url)) = link {
            links.push((key.clone(), label, url));
        }
    }
    links
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

/// ntfy's `click` — what tapping the notification opens.
///
/// Sonarr only ever sends the configured `ClickUrl` (`NtfyProxy.cs:126-129`),
/// which is the same URL for every event. The two cases in front of it are
/// there because the contract carries them and a tap that lands somewhere useful
/// is the whole point of the field: a `ManualInteractionRequired` event carries
/// its own deep link into Scryer and wins, and the operator can nominate a
/// metadata site whose link is specific to the title in the notification.
/// `click_url` remains the fallback, so a configuration that predates this field
/// behaves exactly as it did.
fn click_url(
    req: &PluginNotificationRequest,
    settings: &Settings,
    links: &[(String, &'static str, String)],
) -> Option<String> {
    if let Some(link) = manual_link(req) {
        return Some(link);
    }
    if settings.preferred_metadata_link != PREFERRED_LINK_NONE
        && let Some((_, _, url)) = links
            .iter()
            .find(|(key, _, _)| key == &settings.preferred_metadata_link)
    {
        return Some(url.clone());
    }
    settings
        .click_url
        .clone()
        .or_else(|| req.is_test.then(|| SCRYER_LINK.to_string()))
}

/// ntfy's `icon`: "Only JPEG and PNG images are supported at this time"
/// (docs.ntfy.sh/publish/#icons).
fn icon_url(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if !settings.include_poster {
        return None;
    }
    let poster = poster_url(req)?;
    if !is_absolute_http(&poster) {
        warnings.push(format!(
            "the title's poster is not an absolute http(s) URL and was not attached: {poster}"
        ));
        return None;
    }
    if let Some(extension) = url_extension(&poster)
        && !matches!(extension.as_str(), "jpg" | "jpeg" | "png")
    {
        warnings.push(format!(
            "the title's poster is a .{extension} image; ntfy renders only JPEG and PNG icons, so it may not appear"
        ));
    }
    Some(poster)
}

/// The lower-cased extension of a URL's path, ignoring any query or fragment.
fn url_extension(url: &str) -> Option<String> {
    let path = url
        .split(['?', '#'])
        .next()?
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())?;
    let (_, extension) = path.rsplit_once('.')?;
    (!extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| extension.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

/// The dispatcher stamps a severity on every notification
/// (`dispatcher.rs:895`); the fallback mirrors its own mapping
/// (`dispatcher.rs:920-928`) for a host that does not.
fn severity(req: &PluginNotificationRequest) -> NotificationSeverity {
    if let Some(severity) = req.severity {
        return severity;
    }
    match req.event_type {
        NotificationEventType::Download
        | NotificationEventType::ImportRejected
        | NotificationEventType::SubtitleSearchFailed => NotificationSeverity::Error,
        NotificationEventType::HealthIssue => NotificationSeverity::Warning,
        _ => NotificationSeverity::Info,
    }
}

fn effective_priority(req: &PluginNotificationRequest, settings: &Settings) -> i64 {
    match settings.failure_priority {
        Some(failure)
            if matches!(
                severity(req),
                NotificationSeverity::Warning | NotificationSeverity::Error
            ) =>
        {
            failure
        }
        _ => settings.priority,
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// The parts of a `POST /` body that do not depend on the topic.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Message {
    title: String,
    message: String,
    priority: i64,
    tags: Vec<String>,
    click: Option<String>,
    icon: Option<String>,
}

impl Message {
    /// One JSON publish body. `topic` is the only required field
    /// (docs.ntfy.sh/publish/#publish-as-json); everything else is omitted when
    /// it has nothing to say, so the wire form stays as small as Sonarr's.
    ///
    /// `markdown` is deliberately never set. ntfy's web app has rendered
    /// markdown since server v2.7.0 and the Android app since v1.17.8, but the
    /// iOS app and every older client show the raw source — and this channel has
    /// no need for it, because the links go in `click` and the poster in `icon`
    /// rather than into the message text.
    fn to_json(&self, topic: &str) -> Value {
        let mut body = Map::new();
        body.insert("topic".to_string(), json!(topic));
        if !self.title.is_empty() {
            body.insert("title".to_string(), json!(self.title));
        }
        if !self.message.is_empty() {
            body.insert("message".to_string(), json!(self.message));
        }
        body.insert("priority".to_string(), json!(self.priority));
        if !self.tags.is_empty() {
            body.insert("tags".to_string(), json!(self.tags));
        }
        if let Some(click) = &self.click {
            body.insert("click".to_string(), json!(click));
        }
        if let Some(icon) = &self.icon {
            body.insert("icon".to_string(), json!(icon));
        }
        Value::Object(body)
    }
}

fn build_message(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Message {
    if settings.preferred_metadata_link != PREFERRED_LINK_NONE
        && !settings
            .metadata_links
            .iter()
            .any(|link| link == &settings.preferred_metadata_link)
    {
        warnings.push(format!(
            "preferred_metadata_link '{}' is not among the selected metadata_links; the notification falls back to click_url",
            settings.preferred_metadata_link
        ));
    }

    let links = selected_metadata_links(req, &settings.metadata_links);
    let lines = build_lines(req, settings, &links);
    let body = lines
        .iter()
        .map(Line::render)
        .collect::<Vec<_>>()
        .join("\n");

    Message {
        title: truncate_bytes(&heading(req, settings), MAX_TITLE_BYTES, "title", warnings),
        message: truncate_bytes(&body, MAX_MESSAGE_BYTES, "message", warnings),
        priority: effective_priority(req, settings),
        tags: bounded_tags(&settings.tags, warnings),
        click: click_url(req, settings, &links),
        icon: icon_url(req, settings, warnings),
    }
}

/// ntfy measures `tags` against `messageTagsSizeLimit` (512 bytes) over the
/// whole list. Dropping the tags that do not fit keeps the ones the operator
/// listed first rather than losing the message.
fn bounded_tags(tags: &[String], warnings: &mut Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    for tag in tags {
        let cost = tag.len() + usize::from(!kept.is_empty());
        if used + cost > MAX_TAGS_BYTES {
            dropped += 1;
            continue;
        }
        used += cost;
        kept.push(tag.clone());
    }
    if dropped > 0 {
        warnings.push(format!(
            "{dropped} tag(s) were dropped: ntfy accepts at most {MAX_TAGS_BYTES} bytes of tags per message"
        ));
    }
    kept
}

/// Truncate to a byte budget on a character boundary.
///
/// ntfy's limits are byte counts, not character counts, and both of the ones
/// this channel can hit are silent in different ways: an over-long `message` is
/// turned into a file attachment by the server, and an over-long `title` is
/// rejected with `errHTTPBadRequestTitleTooLarge`. Truncating with a visible
/// ellipsis and a `warnings` entry keeps the notification and tells the operator
/// what happened.
fn truncate_bytes(
    value: &str,
    budget: usize,
    field_name: &str,
    warnings: &mut Vec<String>,
) -> String {
    if value.len() <= budget {
        return value.to_string();
    }
    const ELLIPSIS: &str = "…";
    let keep = budget.saturating_sub(ELLIPSIS.len());
    let mut end = keep.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    warnings.push(format!(
        "the {field_name} was {} bytes and was truncated to ntfy's {budget}-byte limit",
        value.len()
    ));
    format!("{}{ELLIPSIS}", &value[..end])
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// What one topic's `POST` produced.
enum Outcome {
    Delivered {
        status: u16,
        message_id: Option<String>,
    },
    /// The channel itself is misconfigured for this topic. Carries the typed
    /// error so a run in which *every* topic says the same thing can be reported
    /// on the typed lane.
    Misconfigured(PluginError),
    /// The provider said no, for now.
    Rejected {
        status: Option<u16>,
        detail: String,
        provider_status: String,
        retry_after_seconds: Option<i64>,
    },
}

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    let message = build_message(req, &settings, &mut warnings);
    let headers = request_headers(&settings, &mut warnings);

    // A Test-time-only, unauthenticated `GET /v1/health`. It answers the one
    // question Sonarr's test cannot separate from a credential problem: is
    // `server_url` an ntfy server at all? Everything it finds is a warning; the
    // publish immediately afterwards produces the real error.
    if req.is_test {
        warnings.extend(probe_health(&settings.server, &headers));
    }

    let url = format!("{}/", settings.server);
    let mut target_results = Vec::new();
    let mut misconfigurations: Vec<PluginError> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut retry_after_seconds: Option<i64> = None;
    let mut delivery_id = None;

    for topic in &settings.topics {
        let body = match serde_json::to_vec(&message.to_json(topic)) {
            Ok(body) => body,
            Err(error) => {
                return PluginResult::Err(plugin_error(
                    PluginErrorCode::Permanent,
                    "could not encode the ntfy message payload".to_string(),
                    Some(error.to_string()),
                ));
            }
        };

        let outcome = publish(&url, &headers, body, &settings.auth, req.is_test);
        match outcome {
            Outcome::Delivered { status, message_id } => {
                if settings.topics.len() == 1 {
                    delivery_id = message_id;
                }
                target_results.push(PluginNotificationTargetResult {
                    target: topic.clone(),
                    success: true,
                    status: Some(format!("http_{status}")),
                    error: None,
                });
            }
            Outcome::Misconfigured(error) => {
                target_results.push(PluginNotificationTargetResult {
                    target: topic.clone(),
                    success: false,
                    status: error
                        .debug_message
                        .clone()
                        .or_else(|| Some(format!("{:?}", error.code))),
                    error: Some(error.public_message.clone()),
                });
                failures.push(format!("{topic}: {}", error.public_message));
                misconfigurations.push(error);
            }
            Outcome::Rejected {
                status,
                detail,
                provider_status,
                retry_after_seconds: retry_after,
            } => {
                if let Some(retry_after) = retry_after {
                    retry_after_seconds =
                        Some(retry_after_seconds.map_or(retry_after, |seen| seen.max(retry_after)));
                }
                target_results.push(PluginNotificationTargetResult {
                    target: topic.clone(),
                    success: false,
                    status: Some(
                        status
                            .map(|status| format!("http_{status}"))
                            .unwrap_or_else(|| provider_status.clone()),
                    ),
                    error: Some(detail.clone()),
                });
                failures.push(format!("{topic}: {detail}"));
            }
        }
    }

    // Every topic refused the channel's own configuration, and for the same
    // reason: that is a setting the operator must fix, not a delivery that
    // failed, so it goes on the typed lane naming the field. A *partial*
    // failure — which ntfy's per-topic access control makes real, because a
    // token can be granted write on one topic and refused on another — stays on
    // the delivery lane so the topics that did work are still reported.
    if misconfigurations.len() == settings.topics.len()
        && let Some(first) = misconfigurations.first()
        && misconfigurations
            .iter()
            .all(|error| error.code == first.code && error.public_message == first.public_message)
    {
        let mut error = first.clone();
        if settings.topics.len() > 1 {
            error.debug_message = Some(format!(
                "every configured topic failed the same way ({}): {}",
                settings.topics.join(", "),
                error.debug_message.as_deref().unwrap_or("no detail")
            ));
        }
        return PluginResult::Err(error);
    }

    let mut response = if failures.is_empty() {
        ok_response()
    } else {
        error_response(
            failures.join("; "),
            Some(format!(
                "{}/{} ntfy topics failed",
                failures.len(),
                settings.topics.len()
            )),
        )
    };
    response.delivery_id = delivery_id;
    response.retry_after_seconds = retry_after_seconds;
    response.target_results = target_results;
    response.warnings = warnings;
    PluginResult::Ok(response)
}

/// The headers every publish carries.
///
/// The operator's own headers go on first so the ones this channel owns —
/// `Content-Type`, `Accept`, `User-Agent` and `Authorization` — win. An operator
/// header that would have been overwritten is reported rather than silently
/// dropped, except `Authorization` when no credential is configured: forwarding
/// an operator's own `Authorization` line is exactly how an auth proxy in front
/// of ntfy is fed.
fn request_headers(settings: &Settings, warnings: &mut Vec<String>) -> Vec<(String, String)> {
    let auth_header = settings.auth.header();
    let mut owned: Vec<&str> = vec!["content-type", "accept", "user-agent"];
    if auth_header.is_some() {
        owned.push("authorization");
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in &settings.headers {
        if owned.contains(&name.to_ascii_lowercase().as_str()) {
            warnings.push(format!(
                "the configured header {name:?} is set by this channel and was ignored"
            ));
            continue;
        }
        headers.push((name.clone(), value.clone()));
    }

    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    headers.push(("Accept".to_string(), "application/json".to_string()));
    headers.push(("User-Agent".to_string(), USER_AGENT.to_string()));
    if let Some(auth_header) = auth_header {
        headers.push(("Authorization".to_string(), auth_header));
    }
    headers
}

fn publish(
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    auth: &Auth,
    strict: bool,
) -> Outcome {
    let mut request = HttpRequest::new(url).with_method("POST");
    for (name, value) in headers {
        request = request.with_header(name.as_str(), value.as_str());
    }

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            auth,
        ),
        // The host answers a refused or failed egress in-band. On a connection
        // test that is Sonarr's `ValidationFailure("ServerUrl", …)`
        // (`NtfyProxy.cs:102`); on a live send it is the provider being
        // unreachable, which must not be reported to the operator as a broken
        // setting just because the network blinked.
        Err(error) if strict => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "could not reach the ntfy server at {url}: {error}. Check server_url and that Scryer is allowed to reach it."
            ),
            Some(error.to_string()),
        )),
        Err(error) => Outcome::Rejected {
            status: None,
            detail: format!("request failed: {error}"),
            provider_status: "request_failed".to_string(),
            retry_after_seconds: None,
        },
    }
}

/// `GET {server}/v1/health`, Test-time only.
///
/// `apiHealthPath` has no security requirement, so this costs one
/// unauthenticated round trip and tells the operator that `server_url` points at
/// something that is not ntfy *before* the publish blames their credentials.
fn probe_health(server: &str, headers: &[(String, String)]) -> Vec<String> {
    let mut request = HttpRequest::new(format!("{server}{HEALTH_PATH}")).with_method("GET");
    for (name, value) in headers {
        // The probe is unauthenticated by design, and a JSON content type on a
        // GET with no body confuses some reverse proxies.
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "content-type"
        ) {
            continue;
        }
        request = request.with_header(name.as_str(), value.as_str());
    }

    let Ok(response) = http::request::<Vec<u8>>(&request, None) else {
        return Vec::new();
    };

    let status = response.status_code();
    if !(200..300).contains(&status) {
        return vec![format!(
            "GET {server}{HEALTH_PATH} answered HTTP {status}: check that server_url points at an ntfy server"
        )];
    }
    match serde_json::from_slice::<Value>(&response.body())
        .ok()
        .and_then(|body| body.get("healthy").and_then(Value::as_bool))
    {
        Some(true) => Vec::new(),
        Some(false) => vec![format!(
            "the ntfy server at {server} reports itself as unhealthy"
        )],
        None => vec![format!(
            "GET {server}{HEALTH_PATH} did not answer with ntfy's health document: check that server_url points at an ntfy server"
        )],
    }
}

/// ntfy's error body: `{"code":40010,"http":400,"error":"…","link":"…"}`
/// (`server/errors.go`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct NtfyBody {
    code: Option<i64>,
    error: Option<String>,
    link: Option<String>,
    message_id: Option<String>,
    /// Whether the body parsed as a JSON object at all. A `false` here is the
    /// most useful signal this channel has on a 4xx: ntfy answers JSON on every
    /// documented status, so anything else means something that is not ntfy
    /// answered — an authenticating reverse proxy, a captive portal, or an
    /// unrelated service on that origin.
    is_json: bool,
    raw: String,
}

impl NtfyBody {
    fn detail(&self, status: u16) -> String {
        if let Some(text) = self
            .error
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let text = ellipsize(text, MAX_QUOTED_ERROR);
            return match (self.code, self.link.as_deref()) {
                (Some(code), Some(link)) if !link.is_empty() => {
                    format!("{text} (ntfy error {code}, {link})")
                }
                (Some(code), _) => format!("{text} (ntfy error {code})"),
                _ => text,
            };
        }
        match self.raw.trim() {
            "" => format!("HTTP {status}"),
            raw => ellipsize(raw, MAX_QUOTED_ERROR),
        }
    }
}

fn parse_ntfy_body(body: &[u8]) -> NtfyBody {
    let raw = String::from_utf8_lossy(body).to_string();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return NtfyBody {
            raw,
            ..NtfyBody::default()
        };
    };
    NtfyBody {
        code: map.get("code").and_then(Value::as_i64),
        error: map.get("error").and_then(Value::as_str).map(str::to_string),
        link: map.get("link").and_then(Value::as_str).map(str::to_string),
        message_id: map.get("id").and_then(Value::as_str).map(str::to_string),
        is_json: true,
        raw,
    }
}

fn ellipsize(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Sonarr turns every ntfy failure into one exception string and only
/// attributes it to a setting inside `Test` (`NtfyProxy.cs:71-108`). Scryer's
/// typed error lane exists on every send, so the operator is always told which
/// setting to fix — and ntfy's own error `code` is used to pick the field,
/// which is finer-grained than the HTTP status alone.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    auth: &Auth,
) -> Outcome {
    let answer = parse_ntfy_body(body);
    let detail = answer.detail(status);
    let debug = format!("HTTP {status}: {detail}");
    let retry_after = header(headers, "retry-after")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|seconds| *seconds >= 0)
        .map(|seconds| seconds.max(1));

    if (200..300).contains(&status) {
        return Outcome::Delivered {
            status,
            message_id: answer.message_id,
        };
    }

    // A non-2xx that is not ntfy's documented error JSON did not come from ntfy.
    // Naming `access_token` there would send the operator to the wrong setting,
    // which is exactly what Sonarr's 401 branch does.
    if !answer.is_json && !(500..600).contains(&status) && status != 429 {
        return Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "server_url did not answer like an ntfy server (HTTP {status}): {detail}. Check the ntfy URL and anything proxying it."
            ),
            Some(debug),
        ));
    }

    // ntfy's own error codes are more specific than the status: a 400 can be the
    // topic, the priority, or the message this plugin built.
    match answer.code {
        // `errHTTPBadRequestTopicInvalid` / `errHTTPBadRequestTopicDisallowed`.
        Some(40009 | 40010) => {
            return Outcome::Misconfigured(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("ntfy refused the topic: {detail}"),
                Some(debug),
            ));
        }
        // `errHTTPBadRequestPriorityInvalid`.
        Some(40007) => {
            return Outcome::Misconfigured(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("ntfy refused the priority: {detail}"),
                Some(debug),
            ));
        }
        _ => {}
    }

    match status {
        401 | 403 => {
            let (field, explanation) = auth.rejected_field();
            Outcome::Misconfigured(plugin_error(
                PluginErrorCode::AuthFailed,
                format!("{field}: {explanation} (HTTP {status}): {detail}"),
                Some(debug),
            ))
        }
        // ntfy serves the publish route on `/` on every version, so a 404 means
        // `server_url` is wrong or points at something else.
        404 => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "server_url does not expose ntfy's publish endpoint (HTTP 404): {detail}. Check the ntfy URL, including any path prefix."
            ),
            Some(debug),
        )),
        // The message this plugin built is wrong, or too large. The operator has
        // nothing to fix; this is reported as a permanent failure of this
        // message rather than as a retryable one.
        400 | 413 => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::Permanent,
            format!("ntfy rejected the message this plugin built (HTTP {status}): {detail}"),
            Some(debug),
        )),
        // The provider saying "not now": the delivery lane, not the
        // configuration lane. ntfy does not send `Retry-After` today, but a
        // reverse proxy in front of it does and it is the one thing the core
        // could act on.
        _ => Outcome::Rejected {
            status: Some(status),
            detail,
            provider_status: format!("http_{status}"),
            retry_after_seconds: retry_after,
        },
    }
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
    use scryer_plugin_sdk::{
        NotificationMediaUpdateType, PluginNotificationApp, PluginNotificationApplicationUpdate,
        PluginNotificationDownload, PluginNotificationExternalIds, PluginNotificationFile,
        PluginNotificationHealth, PluginNotificationImport, PluginNotificationManualInteraction,
        PluginNotificationMediaUpdate, PluginNotificationRelease, PluginNotificationTitle,
    };

    fn settings() -> Settings {
        Settings {
            server: "https://ntfy.test".to_string(),
            auth: Auth::None,
            priority: 3,
            failure_priority: None,
            topics: vec!["scryer".to_string()],
            tags: Vec::new(),
            click_url: None,
            include_app_name_in_title: false,
            include_poster: false,
            metadata_links: Vec::new(),
            preferred_metadata_link: PREFERRED_LINK_NONE.to_string(),
            headers: Vec::new(),
        }
    }

    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: None,
            occurred_at: Some("2026-09-02T12:00:00+00:00".to_string()),
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
            poster_url: Some("https://images.test/poster.jpg".to_string()),
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

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn message_of(req: &PluginNotificationRequest, settings: &Settings) -> Message {
        build_message(req, settings, &mut Vec::new())
    }

    // -----------------------------------------------------------------
    // Descriptor (M1, M2)
    // -----------------------------------------------------------------

    #[test]
    fn descriptor_keeps_every_june_config_key_and_fixes_the_field_types() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("ntfy must describe a notification provider");
        };

        let by_key = |key: &str| {
            notification
                .config_fields
                .iter()
                .find(|field| field.key == key)
                .unwrap_or_else(|| panic!("{key} must stay a configuration key"))
        };

        // Config keys are a public contract: every June key is still here.
        for key in [
            "server_url",
            "access_token",
            "username",
            "password",
            "priority",
            "topics",
            "tags",
            "click_url",
            "headers",
        ] {
            let _ = by_key(key);
        }

        // M1: the three field types the June port got wrong.
        assert_eq!(by_key("priority").field_type, ConfigFieldType::Select);
        assert_eq!(
            by_key("priority")
                .options
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "3", "4", "5"],
            "Sonarr's NtfyPriority values are unchanged"
        );
        assert_eq!(by_key("topics").field_type, ConfigFieldType::Tag);
        assert!(by_key("topics").required, "Sonarr requires a topic");
        assert_eq!(by_key("tags").field_type, ConfigFieldType::Tag);
        assert_eq!(by_key("headers").field_type, ConfigFieldType::Multiline);

        // M2 and the sibling channels' additions.
        assert_eq!(
            by_key("include_app_name_in_title").field_type,
            ConfigFieldType::Bool
        );
        assert_eq!(
            by_key("include_app_name_in_title").default_value.as_deref(),
            Some("false"),
            "Sonarr's branding is opt-in: summary_title is already event-specific"
        );
        assert_eq!(
            by_key("preferred_metadata_link").default_value.as_deref(),
            Some(PREFERRED_LINK_NONE),
            "an existing configuration must keep using click_url"
        );
    }

    #[test]
    fn descriptor_declares_the_channel_the_host_needs_to_route() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("ntfy must describe a notification provider");
        };
        assert_eq!(
            notification.default_base_url.as_deref(),
            Some(DEFAULT_SERVER_URL)
        );
        assert!(
            notification
                .allowed_hosts
                .contains(&DEFAULT_SERVER_HOST.to_string())
        );
        assert!(notification.capabilities.supports_test);
        assert!(
            notification
                .capabilities
                .event_options
                .supports_upgrade_filter
                && notification
                    .capabilities
                    .event_options
                    .supports_delete_for_upgrade_filter
                && notification
                    .capabilities
                    .event_options
                    .supports_health_warning_filter,
            "every event renders distinctly, so all three per-event filters apply"
        );
        assert!(
            !notification.capabilities.requires_host_process
                && !notification.capabilities.requires_host_filesystem
        );
    }

    // -----------------------------------------------------------------
    // Settings validation (H2)
    // -----------------------------------------------------------------

    #[test]
    fn server_url_must_be_absolute() {
        assert_eq!(
            normalized_server("https://ntfy.example/").unwrap(),
            "https://ntfy.example"
        );
        assert_eq!(
            normalized_server("  https://ntfy.example/ntfy/  ").unwrap(),
            "https://ntfy.example/ntfy",
            "a path prefix is kept: ntfy is often reverse-proxied under one"
        );
        let error = normalized_server("ntfy.example").unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
        assert_eq!(
            normalized_server("https://").unwrap_err().code,
            PluginErrorCode::InvalidConfig
        );
    }

    #[test]
    fn priority_accepts_sonarrs_numbers_and_ntfys_names_and_refuses_the_rest() {
        assert_eq!(
            parse_priority("priority", Some("1"), Some(3)).unwrap(),
            Some(1)
        );
        assert_eq!(
            parse_priority("priority", Some("5"), Some(3)).unwrap(),
            Some(5)
        );
        assert_eq!(
            parse_priority("priority", Some("urgent"), Some(3)).unwrap(),
            Some(5),
            "ntfy accepts the priority by name, so a stored name is honoured"
        );
        assert_eq!(
            parse_priority("priority", Some("Default"), Some(3)).unwrap(),
            Some(3)
        );
        assert_eq!(parse_priority("priority", None, Some(3)).unwrap(), Some(3));
        assert_eq!(
            parse_priority("priority", Some("  "), Some(3)).unwrap(),
            Some(3)
        );

        // `NtfySettings.cs:15`: InclusiveBetween(1, 5).
        for out_of_range in ["0", "6", "-1"] {
            let error = parse_priority("priority", Some(out_of_range), Some(3)).unwrap_err();
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(error.public_message.contains("priority"));
        }
        // The June port's `config_i64` silently substituted the default here.
        let error = parse_priority("priority", Some("loud"), Some(3)).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn failure_priority_defaults_to_using_priority() {
        assert_eq!(
            parse_priority("failure_priority", Some("same"), None).unwrap(),
            None
        );
        assert_eq!(
            parse_priority("failure_priority", None, None).unwrap(),
            None
        );
        assert_eq!(
            parse_priority("failure_priority", Some("5"), None).unwrap(),
            Some(5)
        );
    }

    #[test]
    fn failure_priority_only_applies_to_warning_and_error_severities() {
        let mut settings = settings();
        settings.priority = 2;
        settings.failure_priority = Some(5);

        let mut info = request(NotificationEventType::Grab);
        info.severity = Some(NotificationSeverity::Info);
        assert_eq!(effective_priority(&info, &settings), 2);

        let mut failed = request(NotificationEventType::Download);
        failed.severity = Some(NotificationSeverity::Error);
        assert_eq!(effective_priority(&failed, &settings), 5);

        // With the default "same", severity changes nothing — Sonarr's behaviour.
        settings.failure_priority = None;
        assert_eq!(effective_priority(&failed, &settings), 2);
    }

    #[test]
    fn severity_falls_back_to_the_dispatchers_own_mapping() {
        let mut req = request(NotificationEventType::Download);
        req.severity = None;
        assert_eq!(severity(&req), NotificationSeverity::Error);

        let mut req = request(NotificationEventType::HealthIssue);
        req.severity = None;
        assert_eq!(severity(&req), NotificationSeverity::Warning);

        let mut req = request(NotificationEventType::Rename);
        req.severity = None;
        assert_eq!(severity(&req), NotificationSeverity::Info);
    }

    #[test]
    fn topics_are_validated_the_way_ntfy_validates_them() {
        assert_eq!(
            validated_topics(&["scryer".to_string(), "home-lab_1".to_string()]).unwrap(),
            vec!["scryer".to_string(), "home-lab_1".to_string()]
        );
        // Topics are case-sensitive on ntfy's side, so de-duplication is too.
        assert_eq!(
            validated_topics(&[
                "scryer".to_string(),
                "scryer".to_string(),
                "Scryer".to_string()
            ])
            .unwrap(),
            vec!["scryer".to_string(), "Scryer".to_string()]
        );

        let error = validated_topics(&[]).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("topics"));

        for invalid in ["has space", "dot.topic", "sl/ash", &"a".repeat(65)] {
            let error = validated_topics(&[invalid.to_string()]).unwrap_err();
            assert_eq!(
                error.code,
                PluginErrorCode::InvalidConfig,
                "{invalid} must be refused"
            );
            assert!(error.public_message.contains("topics"));
        }

        // ntfy's current DefaultDisallowedTopics.
        for reserved in ["docs", "settings", "v1", "Login"] {
            let error = validated_topics(&[reserved.to_string()]).unwrap_err();
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        }

        // Sonarr's stale extras are NOT refused: no ntfy server disallows them,
        // and blocking them would break a configuration that works today.
        for allowed in ["announcements", "stats", "mytopic-rw"] {
            assert!(
                validated_topics(&[allowed.to_string()]).is_ok(),
                "{allowed} is allowed by current ntfy"
            );
        }
    }

    #[test]
    fn auth_pairing_follows_sonarrs_validator() {
        let mut warnings = Vec::new();
        assert_eq!(
            resolve_auth(Some("tk_abc".to_string()), None, None, &mut warnings).unwrap(),
            Auth::Token("tk_abc".to_string())
        );
        assert!(warnings.is_empty());

        // A token that is not an ntfy token is a warning, not a refusal.
        let mut warnings = Vec::new();
        assert_eq!(
            resolve_auth(Some("hunter2".to_string()), None, None, &mut warnings).unwrap(),
            Auth::Token("hunter2".to_string())
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("tk_"));

        // The token wins over Basic, and a lone username alongside it is fine —
        // Sonarr only enforces the pairing when AccessToken is empty.
        let mut warnings = Vec::new();
        assert_eq!(
            resolve_auth(
                Some("tk_abc".to_string()),
                Some("user".to_string()),
                None,
                &mut warnings
            )
            .unwrap(),
            Auth::Token("tk_abc".to_string())
        );

        let mut warnings = Vec::new();
        assert_eq!(
            resolve_auth(
                None,
                Some("user".to_string()),
                Some("pass".to_string()),
                &mut warnings
            )
            .unwrap(),
            Auth::Basic("user".to_string(), "pass".to_string())
        );

        let mut warnings = Vec::new();
        let error = resolve_auth(None, Some("user".to_string()), None, &mut warnings).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("password"));

        let mut warnings = Vec::new();
        let error = resolve_auth(None, None, Some("pass".to_string()), &mut warnings).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("username"));

        let mut warnings = Vec::new();
        assert_eq!(
            resolve_auth(None, None, None, &mut warnings).unwrap(),
            Auth::None
        );
    }

    #[test]
    fn auth_headers_match_the_documented_forms() {
        assert_eq!(
            Auth::Token("tk_abc".to_string()).header().as_deref(),
            Some("Bearer tk_abc")
        );
        assert_eq!(
            Auth::Basic("user".to_string(), "pass".to_string())
                .header()
                .as_deref(),
            // base64("user:pass")
            Some("Basic dXNlcjpwYXNz")
        );
        assert_eq!(Auth::None.header(), None);
    }

    #[test]
    fn click_url_must_carry_a_scheme_but_need_not_be_http() {
        assert_eq!(
            validated_click_url(Some("https://scryer.example")).unwrap(),
            Some("https://scryer.example".to_string())
        );
        // ntfy documents mailto:, geo:, ntfy:// and app schemes as click targets.
        for scheme in ["mailto:me@example.com", "geo:0,0?q=x", "ntfy://ntfy.sh/x"] {
            assert!(validated_click_url(Some(scheme)).unwrap().is_some());
        }
        assert_eq!(validated_click_url(None).unwrap(), None);
        assert_eq!(validated_click_url(Some("  ")).unwrap(), None);

        let error = validated_click_url(Some("scryer.example")).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("click_url"));
    }

    #[test]
    fn headers_are_owned_and_validated_strictly_only_at_test_time() {
        let mut warnings = Vec::new();
        let parsed = parse_headers(
            Some("X-Proxy-Token: secret\n\n  X-Other : value  "),
            true,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(
            parsed,
            vec![
                ("X-Proxy-Token".to_string(), "secret".to_string()),
                ("X-Other".to_string(), "value".to_string()),
            ]
        );
        assert!(warnings.is_empty());

        // A header value may itself contain a colon.
        let mut warnings = Vec::new();
        assert_eq!(
            parse_headers(Some("X-Url: https://x.test/a"), true, &mut warnings).unwrap(),
            vec![("X-Url".to_string(), "https://x.test/a".to_string())]
        );

        // Test time: a malformed line is a typed error naming the field.
        let mut warnings = Vec::new();
        let error = parse_headers(Some("not a header"), true, &mut warnings).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("headers"));

        // Live send: the same line is skipped with a warning, because a bad
        // header line is never worth losing a notification over.
        let mut warnings = Vec::new();
        let parsed = parse_headers(Some("not a header\nX-Ok: 1"), false, &mut warnings).unwrap();
        assert_eq!(parsed, vec![("X-Ok".to_string(), "1".to_string())]);
        assert_eq!(warnings.len(), 1);

        // An illegal header name is refused rather than sent.
        let mut warnings = Vec::new();
        assert!(parse_headers(Some("bad name: 1"), true, &mut warnings).is_err());
        let mut warnings = Vec::new();
        assert!(parse_headers(Some(": 1"), true, &mut warnings).is_err());
    }

    #[test]
    fn request_headers_are_owned_by_the_channel_and_the_operator_is_told() {
        let mut authenticated = settings();
        authenticated.auth = Auth::Token("tk_abc".to_string());
        authenticated.headers = vec![
            ("X-Proxy".to_string(), "yes".to_string()),
            ("authorization".to_string(), "Bearer other".to_string()),
        ];

        let mut warnings = Vec::new();
        let headers = request_headers(&authenticated, &mut warnings);
        assert!(headers.contains(&("X-Proxy".to_string(), "yes".to_string())));
        assert!(headers.contains(&("Content-Type".to_string(), "application/json".to_string())));
        assert!(headers.contains(&("Authorization".to_string(), "Bearer tk_abc".to_string())));
        assert!(
            !headers.iter().any(|(_, value)| value == "Bearer other"),
            "the configured credential wins over an operator header"
        );
        assert_eq!(warnings.len(), 1);

        // With no credential configured, the operator's own Authorization line
        // is what feeds an auth proxy in front of ntfy, so it is kept.
        let mut proxied = settings();
        proxied.headers = vec![("Authorization".to_string(), "Bearer proxy".to_string())];
        let mut warnings = Vec::new();
        let headers = request_headers(&proxied, &mut warnings);
        assert!(headers.contains(&("Authorization".to_string(), "Bearer proxy".to_string())));
        assert!(warnings.is_empty());
    }

    #[test]
    fn metadata_link_settings_are_validated() {
        assert_eq!(
            validated_metadata_links(&["TVDb".to_string(), "tvdb".to_string()]).unwrap(),
            vec!["tvdb".to_string()]
        );
        let error = validated_metadata_links(&["letterboxd".to_string()]).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("metadata_links"));

        // Test time: naming an unselected site is refused.
        let error =
            validated_preferred_link(Some("imdb"), &["tvdb".to_string()], true).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("preferred_metadata_link"));
        // Live send: it degrades instead.
        assert_eq!(
            validated_preferred_link(Some("imdb"), &["tvdb".to_string()], false).unwrap(),
            "imdb"
        );
        assert_eq!(
            validated_preferred_link(None, &[], true).unwrap(),
            PREFERRED_LINK_NONE
        );
        assert!(validated_preferred_link(Some("nope"), &[], false).is_err());
    }

    // -----------------------------------------------------------------
    // Payload (H1)
    // -----------------------------------------------------------------

    #[test]
    fn publish_body_is_json_with_the_topic_in_it() {
        let req = request(NotificationEventType::Grab);
        let message = message_of(&req, &settings());
        let body = message.to_json("scryer");

        assert_eq!(body["topic"], "scryer");
        assert_eq!(body["title"], "Grabbed: Example Show");
        assert_eq!(
            body["message"],
            "Grabbed 'Example.Show.S01E01' for 'Example Show'."
        );
        assert_eq!(body["priority"], 3);
        // Nothing empty is sent.
        assert!(body.get("tags").is_none());
        assert!(body.get("click").is_none());
        assert!(body.get("icon").is_none());
        // The message is never sent as markdown: see `Message::to_json`.
        assert!(body.get("markdown").is_none());
    }

    #[test]
    fn tags_travel_as_a_json_array_not_a_comma_string() {
        let mut settings = settings();
        settings.tags = vec!["warning".to_string(), "skull".to_string()];
        let message = message_of(&request(NotificationEventType::Grab), &settings);
        assert_eq!(
            message.to_json("scryer")["tags"],
            json!(["warning", "skull"])
        );
    }

    #[test]
    fn tags_are_bounded_by_ntfys_tag_size_limit() {
        let mut warnings = Vec::new();
        let long = "a".repeat(300);
        let kept = bounded_tags(
            &[long.clone(), long.clone(), "warning".to_string()],
            &mut warnings,
        );
        assert_eq!(kept, vec![long, "warning".to_string()]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("tag"));
    }

    #[test]
    fn the_title_and_message_are_truncated_to_ntfys_byte_limits() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_title = "T".repeat(MAX_TITLE_BYTES + 10);
        req.summary_message = "M".repeat(MAX_MESSAGE_BYTES + 10);

        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);

        assert!(message.title.len() <= MAX_TITLE_BYTES);
        assert!(message.message.len() <= MAX_MESSAGE_BYTES);
        assert!(message.title.ends_with('…'));
        assert!(message.message.ends_with('…'));
        assert_eq!(
            warnings.len(),
            2,
            "both truncations are reported: {warnings:?}"
        );
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let mut warnings = Vec::new();
        // Four-byte characters against a budget that lands mid-character.
        let value = "😀".repeat(10);
        let truncated = truncate_bytes(&value, 20, "message", &mut warnings);
        assert!(truncated.len() <= 20);
        assert!(truncated.ends_with('…'));
        assert!(truncated.chars().all(|c| c == '😀' || c == '…'));
    }

    #[test]
    fn a_short_message_is_untouched() {
        let mut warnings = Vec::new();
        assert_eq!(
            truncate_bytes("hello", 4096, "message", &mut warnings),
            "hello"
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn the_title_is_summary_title_and_the_app_prefix_is_opt_in() {
        let req = request(NotificationEventType::Grab);
        assert_eq!(heading(&req, &settings()), "Grabbed: Example Show");

        let mut branded = settings();
        branded.include_app_name_in_title = true;
        assert_eq!(heading(&req, &branded), "Scryer - Grabbed: Example Show");

        // A blank summary title falls back to the app name, and is not doubled.
        let mut blank = request(NotificationEventType::Grab);
        blank.summary_title = "   ".to_string();
        assert_eq!(heading(&blank, &branded), "Scryer");
    }

    #[test]
    fn an_event_with_nothing_to_say_still_carries_its_heading() {
        let mut req = request(NotificationEventType::Rename);
        req.summary_message = String::new();
        let message = message_of(&req, &settings());
        assert_eq!(message.message, "Grabbed: Example Show");
    }

    #[test]
    fn the_sparse_shape_the_core_sends_today_renders_exactly_the_summary() {
        let req = request(NotificationEventType::Grab);
        let message = message_of(&req, &settings());
        assert_eq!(
            message.message,
            "Grabbed 'Example.Show.S01E01' for 'Example Show'."
        );
    }

    #[test]
    fn a_fully_populated_grab_renders_every_block_the_contract_carries() {
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
            client_name: Some("SABnzbd".to_string()),
            size_bytes: Some(2_147_483_648),
            ..PluginNotificationDownload::default()
        });

        let message = message_of(&req, &settings());
        for expected in [
            "Episode: 1x01 - Pilot",
            "Quality: WEBDL-1080p",
            "Release: Example.Show.S01E01.1080p.WEB-DL",
            "Release Group: GROUP",
            "Indexer: Example Indexer",
            "Size: 2 GB",
            "Client: SABnzbd",
        ] {
            assert!(
                message.message.contains(expected),
                "missing {expected:?} in {:?}",
                message.message
            );
        }
    }

    #[test]
    fn download_renders_a_failure_because_that_is_all_the_event_carries() {
        let mut req = request(NotificationEventType::Download);
        req.summary_message = "Download failed: Example.Show.S01E01".to_string();
        req.severity = Some(NotificationSeverity::Error);
        req.download = Some(PluginNotificationDownload {
            client_name: Some("qBittorrent".to_string()),
            status: Some("failed".to_string()),
            status_message: Some("all files failed to extract".to_string()),
            ..PluginNotificationDownload::default()
        });

        let message = message_of(&req, &settings());
        assert!(message.message.contains("Download failed"));
        assert!(
            message
                .message
                .contains("Status: all files failed to extract")
        );
        assert!(
            !message.message.contains("Destination"),
            "a failed download has no import path"
        );
    }

    #[test]
    fn import_rename_delete_and_health_events_render_their_own_blocks() {
        let mut import = request(NotificationEventType::ImportComplete);
        import.import = Some(PluginNotificationImport {
            dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            ..PluginNotificationImport::default()
        });
        assert!(
            message_of(&import, &settings())
                .message
                .contains("Destination: /media/TV/Example Show/S01E01.mkv")
        );

        let mut rename = request(NotificationEventType::Rename);
        rename.file = Some(PluginNotificationFile {
            primary_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            media_updates: Vec::new(),
        });
        assert!(
            message_of(&rename, &settings())
                .message
                .contains("File: /media")
        );

        let mut deleted = request(NotificationEventType::FileDeleted);
        deleted.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Example Show/old.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });
        assert!(
            message_of(&deleted, &settings())
                .message
                .contains("File: /media/TV/Example Show/old.mkv")
        );

        let mut health = request(NotificationEventType::HealthIssue);
        health.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            details: Some("Indexers unavailable".to_string()),
            ..PluginNotificationHealth::default()
        });
        let rendered = message_of(&health, &settings()).message;
        assert!(rendered.contains("Check: IndexerStatusCheck"));
        assert!(rendered.contains("Detail: Indexers unavailable"));

        let mut update = request(NotificationEventType::ApplicationUpdate);
        update.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            ..PluginNotificationApplicationUpdate::default()
        });
        let rendered = message_of(&update, &settings()).message;
        assert!(rendered.contains("Previous Version: 0.19.7"));
        assert!(rendered.contains("New Version: 0.19.8"));
    }

    #[test]
    fn episode_display_is_composed_when_the_core_did_not_fill_it() {
        let mut req = request(NotificationEventType::Grab);
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

        // A daily episode has no episode number, so the air date carries it.
        let mut daily = request(NotificationEventType::Grab);
        daily.episode = Some(PluginNotificationEpisode {
            air_date: Some("2026-09-02".to_string()),
            title: Some("Tonight".to_string()),
            ..PluginNotificationEpisode::default()
        });
        assert_eq!(
            episode_display(&daily).as_deref(),
            Some("2026-09-02 - Tonight")
        );
    }

    #[test]
    fn unknown_and_unhandled_events_never_fail() {
        for event_type in general_notification_events() {
            let req = request(event_type);
            let message = message_of(&req, &settings());
            assert!(
                !message.message.is_empty(),
                "{event_type:?} rendered an empty message"
            );
            assert!(message.to_json("scryer")["topic"] == "scryer");
        }
    }

    // -----------------------------------------------------------------
    // Click target and icon
    // -----------------------------------------------------------------

    #[test]
    fn click_falls_back_to_the_configured_url_which_is_sonarrs_behaviour() {
        let mut settings = settings();
        settings.click_url = Some("https://scryer.example".to_string());
        let req = request(NotificationEventType::Grab);
        assert_eq!(
            click_url(&req, &settings, &[]).as_deref(),
            Some("https://scryer.example")
        );
    }

    #[test]
    fn the_preferred_metadata_link_becomes_the_click_target() {
        let mut settings = settings();
        settings.click_url = Some("https://scryer.example".to_string());
        settings.metadata_links = vec!["tvdb".to_string(), "imdb".to_string()];
        settings.preferred_metadata_link = "imdb".to_string();

        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let links = selected_metadata_links(&req, &settings.metadata_links);
        assert_eq!(
            click_url(&req, &settings, &links).as_deref(),
            Some("https://www.imdb.com/title/tt0903747")
        );

        // A title with no id for the preferred site falls back to click_url.
        let bare = request(NotificationEventType::Grab);
        assert_eq!(
            click_url(&bare, &settings, &[]).as_deref(),
            Some("https://scryer.example")
        );
    }

    #[test]
    fn a_manual_interaction_link_wins_over_everything() {
        let mut settings = settings();
        settings.click_url = Some("https://scryer.example".to_string());
        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            link: Some("https://scryer.example/queue/1".to_string()),
            reason: Some("Waiting for a decision".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        assert_eq!(
            click_url(&req, &settings, &[]).as_deref(),
            Some("https://scryer.example/queue/1")
        );

        // A relative link is a dead tap and is not offered.
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            link: Some("/queue/1".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        assert_eq!(
            click_url(&req, &settings, &[]).as_deref(),
            Some("https://scryer.example")
        );
    }

    #[test]
    fn a_test_gets_a_click_target_and_a_link_line() {
        let req = request(NotificationEventType::Test);
        let message = message_of(&req, &settings());
        assert_eq!(message.click.as_deref(), Some(SCRYER_LINK));
        assert!(message.message.contains(SCRYER_LINK));
    }

    #[test]
    fn metadata_links_follow_the_titles_facet() {
        let mut series = request(NotificationEventType::Grab);
        series.title = Some(series_title());
        let links = selected_metadata_links(
            &series,
            &[
                "trakt".to_string(),
                "tvmaze".to_string(),
                "tvdb".to_string(),
            ],
        );
        assert_eq!(
            links
                .iter()
                .map(|(_, label, url)| (*label, url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Trakt", "https://trakt.tv/search/tvdb/12345?id_type=show"),
                ("TVMaze", "https://www.tvmaze.com/shows/82"),
                ("TVDb", "https://thetvdb.com/?tab=series&id=12345"),
            ]
        );

        let mut movie = request(NotificationEventType::Grab);
        movie.title = Some(movie_title());
        let links = selected_metadata_links(&movie, &["trakt".to_string(), "tmdb".to_string()]);
        assert_eq!(
            links
                .iter()
                .map(|(_, label, url)| (*label, url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Trakt", "https://trakt.tv/search/tmdb/603?id_type=movie"),
                ("TMDb", "https://www.themoviedb.org/movie/603"),
            ]
        );

        // A selected site with no id renders nothing rather than a dead URL.
        let mut bare = request(NotificationEventType::Grab);
        bare.title = Some(PluginNotificationTitle {
            external_ids: PluginNotificationExternalIds::default(),
            ..series_title()
        });
        assert!(selected_metadata_links(&bare, &["imdb".to_string()]).is_empty());
    }

    #[test]
    fn the_poster_becomes_ntfys_icon_only_when_asked_for() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        assert!(message_of(&req, &settings()).icon.is_none());

        let mut with_poster = settings();
        with_poster.include_poster = true;
        assert_eq!(
            message_of(&req, &with_poster).icon.as_deref(),
            Some("https://images.test/poster.jpg")
        );

        // A relative poster is a dead image and is dropped with a warning.
        req.title = Some(PluginNotificationTitle {
            poster_url: Some("/images/poster.jpg".to_string()),
            ..series_title()
        });
        let mut warnings = Vec::new();
        assert!(
            build_message(&req, &with_poster, &mut warnings)
                .icon
                .is_none()
        );
        assert_eq!(warnings.len(), 1);

        // ntfy renders only JPEG and PNG, so anything else is warned about but
        // still sent — the operator's server may transcode.
        req.title = Some(PluginNotificationTitle {
            poster_url: Some("https://images.test/poster.webp".to_string()),
            ..series_title()
        });
        let mut warnings = Vec::new();
        assert_eq!(
            build_message(&req, &with_poster, &mut warnings)
                .icon
                .as_deref(),
            Some("https://images.test/poster.webp")
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("JPEG and PNG"));
    }

    #[test]
    fn url_extension_ignores_the_query_string() {
        assert_eq!(
            url_extension("https://x.test/a.JPG?w=1").as_deref(),
            Some("jpg")
        );
        assert_eq!(url_extension("https://x.test/a").as_deref(), None);
        assert_eq!(url_extension("https://x.test/").as_deref(), None);
    }

    // -----------------------------------------------------------------
    // Error classification (H2)
    // -----------------------------------------------------------------

    fn classify(status: u16, body: &str, auth: Auth) -> Outcome {
        classify_response(status, &headers(&[]), body.as_bytes(), &auth)
    }

    fn misconfiguration(outcome: Outcome) -> PluginError {
        match outcome {
            Outcome::Misconfigured(error) => error,
            _ => panic!("expected a typed configuration failure"),
        }
    }

    #[test]
    fn a_2xx_is_a_delivery_and_carries_ntfys_message_id() {
        let outcome = classify(200, r#"{"id":"AbCdEf","topic":"scryer"}"#, Auth::None);
        let Outcome::Delivered { status, message_id } = outcome else {
            panic!("a 200 is a delivery");
        };
        assert_eq!(status, 200);
        assert_eq!(message_id.as_deref(), Some("AbCdEf"));
    }

    #[test]
    fn an_unauthorized_response_names_the_credential_that_is_configured() {
        // `NtfyProxy.cs:85-89`: a token is configured, so the token is blamed.
        let error = misconfiguration(classify(
            401,
            r#"{"code":40101,"http":401,"error":"unauthorized"}"#,
            Auth::Token("tk_abc".to_string()),
        ));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("access_token"));

        // `NtfyProxy.cs:91-95`: Basic credentials are configured.
        let error = misconfiguration(classify(
            403,
            r#"{"code":40301,"http":403,"error":"forbidden"}"#,
            Auth::Basic("user".to_string(), "pass".to_string()),
        ));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("username"));

        // `NtfyProxy.cs:97-98`: neither is configured.
        let error = misconfiguration(classify(
            401,
            r#"{"code":40101,"http":401,"error":"unauthorized"}"#,
            Auth::None,
        ));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("authorization"));
    }

    #[test]
    fn a_404_and_a_non_ntfy_body_both_name_the_server_url() {
        let error = misconfiguration(classify(
            404,
            r#"{"code":40401,"http":404,"error":"page not found"}"#,
            Auth::None,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));

        // An authenticating reverse proxy answering HTML is not ntfy saying no.
        let error = misconfiguration(classify(401, "<html>Sign in</html>", Auth::None));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
    }

    #[test]
    fn ntfys_error_code_attributes_a_400_more_precisely_than_the_status() {
        let error = misconfiguration(classify(
            400,
            r#"{"code":40010,"http":400,"error":"invalid request: topic name is not allowed","link":"https://ntfy.sh/docs/publish/"}"#,
            Auth::None,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("topic"));
        assert!(
            error.public_message.contains("40010"),
            "the operator sees ntfy's own code: {}",
            error.public_message
        );

        let error = misconfiguration(classify(
            400,
            r#"{"code":40007,"http":400,"error":"invalid priority parameter"}"#,
            Auth::None,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("priority"));

        // Anything else on a 400 is the message this plugin built.
        let error = misconfiguration(classify(
            400,
            r#"{"code":40024,"http":400,"error":"invalid request: request body must be valid JSON"}"#,
            Auth::None,
        ));
        assert_eq!(error.code, PluginErrorCode::Permanent);
    }

    #[test]
    fn a_413_is_permanent_because_retrying_the_same_message_cannot_help() {
        let error = misconfiguration(classify(
            413,
            r#"{"code":41303,"http":413,"error":"JSON body too large"}"#,
            Auth::None,
        ));
        assert_eq!(error.code, PluginErrorCode::Permanent);
    }

    #[test]
    fn a_429_is_a_delivery_failure_carrying_retry_after() {
        let outcome = classify_response(
            429,
            &headers(&[("Retry-After", "42")]),
            br#"{"code":42901,"http":429,"error":"limit reached: too many requests"}"#,
            &Auth::None,
        );
        let Outcome::Rejected {
            status,
            retry_after_seconds,
            provider_status,
            detail,
        } = outcome
        else {
            panic!("a 429 is the provider saying not now");
        };
        assert_eq!(status, Some(429));
        assert_eq!(retry_after_seconds, Some(42));
        assert_eq!(provider_status, "http_429");
        assert!(detail.contains("too many requests"));
        assert!(detail.contains("42901"));

        // ntfy itself does not send Retry-After; the field is then absent.
        let outcome = classify(
            429,
            r#"{"code":42901,"http":429,"error":"limit reached"}"#,
            Auth::None,
        );
        let Outcome::Rejected {
            retry_after_seconds,
            ..
        } = outcome
        else {
            panic!("a 429 is the provider saying not now");
        };
        assert_eq!(retry_after_seconds, None);
    }

    #[test]
    fn a_507_and_a_5xx_are_delivery_failures_not_configuration_errors() {
        for (status, body) in [
            (
                507u16,
                r#"{"code":50701,"http":507,"error":"cannot publish to UnifiedPush topic without previously active subscriber"}"#,
            ),
            (500, "upstream exploded"),
            (502, "<html>Bad Gateway</html>"),
        ] {
            let outcome = classify(status, body, Auth::None);
            assert!(
                matches!(outcome, Outcome::Rejected { .. }),
                "HTTP {status} must stay on the delivery lane"
            );
        }
    }

    #[test]
    fn upstream_error_text_is_quoted_but_bounded() {
        let long = "x".repeat(MAX_QUOTED_ERROR * 2);
        let body = json!({ "code": 40024, "http": 400, "error": long }).to_string();
        let error = misconfiguration(classify(400, &body, Auth::None));
        assert!(error.public_message.chars().count() < MAX_QUOTED_ERROR + 120);
        assert!(error.public_message.contains('…'));
    }

    #[test]
    fn an_empty_body_still_produces_a_usable_detail() {
        let outcome = classify(503, "", Auth::None);
        let Outcome::Rejected { detail, .. } = outcome else {
            panic!("a 503 is a delivery failure");
        };
        assert_eq!(detail, "HTTP 503");
    }
}
