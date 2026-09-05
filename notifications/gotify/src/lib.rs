//! Gotify push notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Gotify notification (`src/NzbDrone.Core/Notifications/Gotify/`) is a
//! small channel with three parts that matter: a fixed per-event title, an
//! optional series poster rendered both as a markdown image and as
//! `client::notification.bigImageUrl`, and an operator-selected set of metadata
//! links appended as markdown, one of which becomes
//! `client::notification.click.url` (`Gotify.cs:131-207`). Everything else it
//! gets wrong quietly: every failure that is not a 401 becomes one opaque
//! "Unable to connect to Gotify" exception (`GotifyProxy.cs:39-46`), and the
//! settings rules are only ever checked by the settings form
//! (`GotifySettings.cs:11-41`).
//!
//! The June port copied that shape and then reported its *own* configuration
//! checks as delivery failures, which tells the operator "a notification failed
//! to send" when the truth is "preferred_metadata_link names a site you did not
//! select".
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * `metadata_links` (a `Tag` field with options) and `preferred_metadata_link`
//!   (a `Select`) carry Sonarr's two settings, generalised from Sonarr's
//!   series-only world to Scryer's facets — TVDb/TVMaze/Trakt for series,
//!   TMDb/IMDb for movies, and the anime id set when the contract carries one.
//!   The stored values are unchanged;
//! * `priority` becomes a `Select` over Sonarr's Min/Low/Normal/High (0/2/5/8)
//!   plus Gotify's own "use the application's default priority", instead of a
//!   free `Number` an operator could set to `high` and have silently delivered
//!   as `5`;
//! * every configuration problem is a typed `PluginError` naming the field —
//!   `server`, `app_token`, `priority`, `failure_priority`, `metadata_links`,
//!   `preferred_metadata_link` — instead of a fake delivery failure;
//! * Gotify's own error JSON (`{error, errorCode, errorDescription}`) is parsed
//!   and attributed: a rejected token is `AuthFailed` on `app_token`, a URL that
//!   is not a Gotify server is `InvalidConfig` on `server`, and a `400` is a
//!   `Permanent` failure of the message *this plugin* built. Sonarr collapses
//!   all of these into one exception string;
//! * the body is enriched per event from the structured blocks the contract
//!   carries (episode, quality, release, indexer, client, size, paths, health,
//!   version) rather than being `summary_message` alone;
//! * text interpolated into a markdown message is escaped. Sonarr does not, so a
//!   release name containing `*` or `_` currently renders as emphasis and loses
//!   the characters — and Gotify's own documentation warns that markdown
//!   assembled from external text is an injection surface (see below).
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")`. Gotify's failures are three different lanes in Scryer's
//! contract: a `401`/`403` is the application token, a `404` or a body that is
//! not Gotify's error JSON is the `server` URL (an auth proxy or an unrelated
//! service is answering), a `400` is the payload this plugin built, and only a
//! `429`/`5xx` is the provider saying "not now".
//!
//! # Upstream reference
//!
//! Read 2026-09-02:
//!
//! * <https://gotify.net/docs/pushmsg> — `POST /message`, authenticated with the
//!   `X-Gotify-Key` header in every documented example. Only `message` is
//!   required.
//! * <https://gotify.net/docs/priority> — priority is open-ended and the Android
//!   client maps it to notification behaviour: `0` no notification, `1-3` icon
//!   only, `4-7` icon and sound, `8-10` icon, sound and vibration. The WebUI
//!   plays a sound from `4`. "When a message is pushed without a priority, the
//!   default priority of the application is used."
//! * <https://gotify.net/docs/msgextras> — `client::display.contentType`
//!   (`text/plain` default, `text/markdown`; gotify/server ui since v2.0.5,
//!   gotify/android since v2.0.7), `client::notification.click.url`
//!   (gotify/android since v2.0.10), `client::notification.bigImageUrl`
//!   (gotify/android since v2.3.0), `android::action.onReceive.intentUrl`
//!   (gotify/android since v2.0.11). Extras "are only accepted in POST /message
//!   requests with application/json content-type". The page also carries the
//!   warning this module's escaping answers: markdown images "will be
//!   automatically downloaded when the message is viewed […] if part of the
//!   message is interpolated from a malicious external source, the attacker
//!   could inject malformed markdown which leads to information disclosure".
//! * <https://github.com/gotify/server/blob/master/docs/spec.json> (REST-API
//!   2.1.0) — the `Error` model (`error`, `errorCode`, `errorDescription`),
//!   `POST /message` responses 200/400/401/403, the `Message` model's `id`, and
//!   the security definitions: `appTokenHeader` (`X-Gotify-Key`) and
//!   `appTokenQuery` (`?token=`) are both still accepted. `GET /version` carries
//!   no security requirement at all.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationSeverity,
    PluginNotificationEpisode, current_sdk_constraint,
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

const PROVIDER_TYPE: &str = "gotify";
const USER_AGENT: &str = concat!("scryer-gotify-plugin/", env!("CARGO_PKG_VERSION"));

/// The link shown on a test message, standing in for Sonarr's `sonarr.tv`
/// (`Gotify.cs:113-114`). Sonarr puts a link on the test so the operator can see
/// that link rendering and the click target work at all.
const SCRYER_LINK: &str = "https://github.com/scryer-media/scryer";

/// `client::display.contentType` support table
/// (<https://gotify.net/docs/msgextras>): the WebUI renders markdown from
/// gotify/server ui v2.0.5. Older servers show the raw markdown source, which is
/// worth a warning but never worth refusing to deliver.
const MARKDOWN_WEBUI_MIN_VERSION: (u64, u64, u64) = (2, 0, 5);

/// Gotify truncates nothing and documents no length limit on `message` or
/// `title`, so — unlike Telegram, Pushover or Discord — this channel has no
/// truncation path. The one length this module imposes is on the *upstream
/// error text* it quotes back to the operator.
const MAX_QUOTED_ERROR: usize = 300;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// "use the application's default priority" — Gotify's own behaviour for a
/// message pushed without a `priority` field. Sonarr always sends a number and
/// so has no way to express it.
const PRIORITY_APPLICATION_DEFAULT: &str = "app";

/// `GotifyPriority.cs`, annotated with what the clients actually do with each
/// value (<https://gotify.net/docs/priority>). Sonarr renders these as a select;
/// the June port made the field a free `Number`, so an operator could store
/// `high` and have it silently delivered as `5`. The stored values are
/// unchanged.
const PRIORITY_OPTIONS: &[(&str, &str)] = &[
    (
        PRIORITY_APPLICATION_DEFAULT,
        "Application default (set in Gotify)",
    ),
    ("0", "Min (0) - no notification"),
    ("2", "Low (2) - icon only"),
    ("5", "Normal (5) - icon and sound"),
    ("8", "High (8) - icon, sound and vibration"),
];

/// "use `priority`" — the default, and Sonarr's only behaviour.
const FAILURE_PRIORITY_SAME: &str = "same";

/// The priority used when the event's severity is `Warning` or `Error`.
///
/// Sonarr sends one priority for every event, so a failed download is as quiet
/// as a rename. Scryer's dispatcher stamps a severity on every notification
/// (`dispatcher.rs:895`, `:920-928`), which is enough to make failures louder —
/// but only when the operator asks, because overriding a deliberate `Min (0)`
/// would un-mute exactly the channel they muted.
const FAILURE_PRIORITY_OPTIONS: &[(&str, &str)] = &[
    (FAILURE_PRIORITY_SAME, "Same as Priority"),
    ("0", "Min (0) - no notification"),
    ("2", "Low (2) - icon only"),
    ("5", "Normal (5) - icon and sound"),
    ("8", "High (8) - icon, sound and vibration"),
    ("10", "Max (10) - icon, sound and vibration"),
];

/// The largest priority with a documented client behaviour
/// (<https://gotify.net/docs/priority>: "8 - 10"). Gotify itself stores any
/// integer, so this is a guard against a typo, not a protocol rule.
const MAX_DOCUMENTED_PRIORITY: i64 = 10;

/// Sonarr's four options (`MetadataLinkType.cs`) plus the ones its series-only
/// world cannot express.
///
/// Scryer's `PluginNotificationTitle` carries a facet and a full external-id
/// set, so a movie library gets TMDb/IMDb and an anime library gets its own
/// trackers instead of four dead options.
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

/// Sonarr has no "no click target" option: `PreferredMetadataLink` defaults to
/// TVDb and its validator only fires once some link is selected
/// (`GotifySettings.cs:29-40`). Selecting links without making one of them the
/// tap target is a reasonable thing to want, so it is expressible here.
const PREFERRED_LINK_NONE: &str = "none";

fn preferred_link_options() -> Vec<(&'static str, &'static str)> {
    let mut options = vec![(PREFERRED_LINK_NONE, "None")];
    options.extend_from_slice(METADATA_LINK_OPTIONS);
    options
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Gotify".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // Gotify is self-hosted: there is no vendor endpoint to prefill and
            // no host set to allowlist. The operator's `server` is the only
            // origin this channel ever reaches.
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                // `client::display.contentType: text/markdown`.
                supports_rich_text: true,
                // The poster travels as a URL in two places: a markdown image
                // for the WebUI and `client::notification.bigImageUrl` for the
                // Android client. No bytes are uploaded.
                supports_images: true,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![NotificationDeliveryMode::Push],
                payload_formats: vec![
                    NotificationPayloadFormat::PlainText,
                    NotificationPayloadFormat::Markdown,
                ],
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
            "server",
            "Server",
            true,
            None,
            Some("Gotify server URL, for example https://gotify.example."),
        ),
        field(
            "app_token",
            "App Token",
            ConfigFieldType::Password,
            true,
            None,
            Some(
                "The Gotify application token. Gotify 3 shows it only once, when the application is created or its token is rotated.",
            ),
        ),
        select_field("priority", "Priority", Some("5"), PRIORITY_OPTIONS),
        select_field(
            "failure_priority",
            "Failure Priority",
            Some(FAILURE_PRIORITY_SAME),
            FAILURE_PRIORITY_OPTIONS,
        ),
        // The key is Sonarr's and is a public contract; the label is not, and
        // Scryer's libraries are not all series.
        field(
            "include_series_poster",
            "Include Poster",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Adds the title's poster as a markdown image and as Gotify's big notification image. This switches the message to markdown.",
            ),
        ),
        tag_field(
            "metadata_links",
            "Metadata Links",
            METADATA_LINK_OPTIONS,
            Some(
                "Metadata sites to link at the end of the message. Only the sites the title actually has an id for are rendered.",
            ),
        ),
        select_field(
            "preferred_metadata_link",
            "Preferred Metadata Link",
            Some("tvdb"),
            &preferred_link_options(),
        ),
    ]
}

/// A multi-value field with a fixed option set.
///
/// Scryer's notification settings UI renders a `Tag` field as a plain
/// comma-separated text input (`settings-notifications-section.tsx:242-345`
/// handles `BOOL`, `SELECT`, `MULTILINE`, `NUMBER` and `PASSWORD`, and falls
/// through to a text input for everything else), so the options are descriptor
/// documentation today rather than a chip picker. The stored value is unchanged
/// either way, which is what keeps existing configurations parsing.
fn tag_field(
    key: &str,
    label: &str,
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
        ..field(key, label, ConfigFieldType::Tag, false, None, help_text)
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Everything the renderer and the sender need from configuration, resolved and
/// validated once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    server: String,
    app_token: String,
    /// `None` means "send no `priority` field", which Gotify documents as "the
    /// default priority of the application is used".
    priority: Option<i64>,
    /// `None` means "use `priority`".
    failure_priority: Option<i64>,
    include_poster: bool,
    metadata_links: Vec<String>,
    preferred_metadata_link: String,
}

impl Settings {
    /// `strict` is the Test-time posture. Rules Gotify itself will enforce (a
    /// server URL that is not a URL, a priority that is not a number) are errors
    /// on every send. Sonarr's cross-field rule — the preferred link must be one
    /// of the selected links (`GotifySettings.cs:29-40`) — is refused at Test
    /// time and degraded to a warning on a live send, because a click target is
    /// never worth losing a notification over.
    fn from_config(strict: bool) -> Result<Self, PluginError> {
        let server = normalized_server(required_config("server").map_err(config_error)?)?;
        let app_token = required_config("app_token").map_err(config_error)?;

        let priority = parse_priority("priority", PRIORITY_APPLICATION_DEFAULT, Some(5))?;
        let failure_priority = parse_priority("failure_priority", FAILURE_PRIORITY_SAME, None)?;

        let metadata_links = validated_metadata_links(&config_csv("metadata_links"))?;
        let preferred_metadata_link = validated_preferred_link(
            config_value("preferred_metadata_link").as_deref(),
            &metadata_links,
            strict,
        )?;

        Ok(Self {
            server,
            app_token,
            priority,
            failure_priority,
            include_poster: config_bool("include_series_poster"),
            metadata_links,
            preferred_metadata_link,
        })
    }
}

/// `GotifySettingsValidator` (`GotifySettings.cs:15`): `RuleFor(c =>
/// c.Server).IsValidUrl()`.
///
/// Sonarr can only say this through its settings form. Here it is a typed
/// `InvalidConfig` naming the field, because the alternative — the June port's
/// bare `trim_end_matches('/')` — turns `gotify.example` into a request the host
/// refuses with a message about an unsupported scheme.
fn normalized_server(raw: String) -> Result<String, PluginError> {
    let trimmed = raw.trim().trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "server must be an absolute http:// or https:// URL, for example https://gotify.example"
                .to_string(),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    if trimmed.len() <= lower.find("//").map(|at| at + 2).unwrap_or(0) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "server has no host, for example https://gotify.example".to_string(),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    Ok(trimmed.to_string())
}

/// A priority setting, which is either the sentinel that means "do not send
/// one", a whole number in Gotify's documented range, or a configuration error.
///
/// `config_i64` — what the June port used — silently substitutes the default, so
/// `priority = "high"` became Normal and nobody was told.
fn parse_priority(
    key: &'static str,
    sentinel: &str,
    default_value: Option<i64>,
) -> Result<Option<i64>, PluginError> {
    parse_priority_value(key, sentinel, config_value(key).as_deref(), default_value)
}

/// [`parse_priority`] with the configured value supplied, so the rules are a
/// pure function and can be tested without a host.
fn parse_priority_value(
    key: &'static str,
    sentinel: &str,
    raw: Option<&str>,
    default_value: Option<i64>,
) -> Result<Option<i64>, PluginError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(default_value);
    };
    if raw.eq_ignore_ascii_case(sentinel) {
        return Ok(None);
    }
    let priority = raw.parse::<i64>().map_err(|error| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("{key} must be a whole number, or {sentinel:?}; got {raw:?}"),
            Some(error.to_string()),
        )
    })?;
    if !(0..=MAX_DOCUMENTED_PRIORITY).contains(&priority) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "{key} must be between 0 and {MAX_DOCUMENTED_PRIORITY}; got {priority}. Gotify's clients map 0 to no notification, 1-3 to an icon, 4-7 to an icon and sound, and 8-10 to an icon, sound and vibration."
            ),
            None,
        ));
    }
    Ok(Some(priority))
}

/// `GotifySettings.cs:18-27`: every selected link must be a known option.
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

/// `GotifySettings.cs:29-40`: "Must be a selected link", enforced only when some
/// link is selected. That cross-field rule is a Test-time error and a live-send
/// warning; an unknown *value* is an error either way.
fn validated_preferred_link(
    raw: Option<&str>,
    metadata_links: &[String],
    strict: bool,
) -> Result<String, PluginError> {
    let value = raw
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "tvdb".to_string());

    let options = preferred_link_options();
    if !options.iter().any(|(key, _)| *key == value) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("preferred_metadata_link is not a valid value: {value}"),
            Some(format!("known values: {}", option_keys(&options))),
        ));
    }

    if strict
        && value != PREFERRED_LINK_NONE
        && !metadata_links.is_empty()
        && !metadata_links.iter().any(|link| link == &value)
    {
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

fn option_keys(options: &[(&str, &str)]) -> String {
    options
        .iter()
        .map(|(key, _)| *key)
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Message model
//
// Gotify documents no length limit on `message`, so — unlike every other push
// channel in this repository — nothing here truncates. What the model buys
// instead is a single place where markdown is decided: a line knows whether it
// needs markup, and the text lines are only escaped once some other line has
// forced `contentType: text/markdown`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Plain(String),
    Labeled(&'static str, String),
    /// `[label](url)` — forces markdown.
    Link {
        label: String,
        url: String,
    },
    /// `![](url)` — forces markdown. Sonarr writes the same line
    /// (`Gotify.cs:153`).
    Image(String),
}

impl Line {
    /// Whether rendering this line needs `contentType: text/markdown`.
    ///
    /// Sonarr sets the same flag imperatively as it appends
    /// (`Gotify.cs:133,150,160`). Deriving it from the lines keeps the flag and
    /// the body from disagreeing, which is what decides whether the text lines
    /// have to be escaped.
    fn needs_markdown(&self) -> bool {
        matches!(self, Line::Link { .. } | Line::Image(_))
    }

    fn render(&self, markdown: bool) -> String {
        let text = |value: &str| {
            if markdown {
                markdown_escape(value)
            } else {
                value.to_string()
            }
        };
        match self {
            Line::Plain(value) => text(value),
            Line::Labeled(label, value) => format!("{}: {}", text(label), text(value)),
            // A link label is markdown-escaped but the destination is
            // percent-escaped: a backslash inside `(...)` is not an escape, and
            // an unescaped space or bracket ends the destination early.
            Line::Link { label, url } => {
                format!("[{}]({})", markdown_escape(label), markdown_url(url))
            }
            Line::Image(url) => format!("![]({})", markdown_url(url)),
        }
    }
}

/// Backslash-escape the characters that can start markup.
///
/// CommonMark (gotify/android) and GitHub Flavored Markdown (gotify/server ui)
/// both honour a backslash before ASCII punctuation, and both render the escaped
/// character as itself — so this is invisible to the reader and only ever
/// applied when the message is actually being sent as markdown.
///
/// The set is deliberately the inline constructs rather than every ASCII
/// punctuation mark: emphasis and code spans (`*`, `_`, backtick), links and
/// images (`[`, `]`), autolinks and raw HTML (`<`, `>`), GFM strikethrough (`~`)
/// and tables (`|`), headings (`#`), and the escape character itself. A
/// release name like `Example.Show.S01E01-GROUP` therefore stays readable.
fn markdown_escape(value: &str) -> String {
    const SPECIALS: [char; 11] = ['\\', '`', '*', '_', '[', ']', '<', '>', '~', '|', '#'];
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if SPECIALS.contains(&character) {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// Percent-encode the characters that would end a markdown link destination
/// early. Everything else is left alone so the URL stays recognisable.
fn markdown_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for character in url.chars() {
        match character {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '"' => out.push_str("%22"),
            _ => out.push(character),
        }
    }
    out
}

/// Render the lines into `message`, deciding `contentType` from what they need.
fn render_message(lines: &[Line]) -> (String, bool) {
    let markdown = lines.iter().any(Line::needs_markdown);
    let body = lines
        .iter()
        .map(|line| line.render(markdown))
        .collect::<Vec<_>>()
        .join("\n");
    (body, markdown)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// `Gotify.cs:32-80` sends a fixed constant per event ("Episode Grabbed",
/// "Import Complete", …) and hands the event's own prose to the proxy. Scryer's
/// dispatcher already composes an event-specific, title-bearing heading in
/// `summary_title` ("Grabbed: Example Show"), which is strictly more informative
/// in a push notification whose title is what the lock screen shows.
///
/// The title is **not** markdown-escaped: `contentType` governs the message
/// only, and gotify/server's WebUI renders the title through a plain
/// `Typography` element (`ui/src/message/Message.tsx`), so a backslash there
/// would be visible.
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

fn build_lines(
    req: &PluginNotificationRequest,
    settings: &Settings,
    links: &[(String, &'static str, String)],
    warnings: &mut Vec<String>,
) -> Vec<Line> {
    let mut lines = Vec::new();

    let message = req.summary_message.trim();
    if !message.is_empty() {
        lines.push(Line::Plain(message.to_string()));
    }

    lines.extend(detail_lines(req));

    if let Some(poster) = poster_line(req, settings, warnings) {
        lines.push(Line::Image(poster));
    }

    for (_, label, url) in links {
        lines.push(Line::Link {
            label: (*label).to_string(),
            url: url.clone(),
        });
    }

    if req.is_test {
        lines.push(Line::Link {
            label: req.app.name.trim().to_string(),
            url: SCRYER_LINK.to_string(),
        });
    }

    // Gotify requires `message`; an event whose summary and blocks are all blank
    // still has a heading.
    if lines.is_empty() {
        lines.push(Line::Plain(heading(req)));
    }

    lines
}

/// `Gotify.cs:146-156`. Sonarr picks the series' poster cover; the contract
/// carries `poster_url` with `background_url` as the fallback
/// (`notify_common::poster_url`).
fn poster_line(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if !settings.include_poster {
        return None;
    }
    let poster = poster_url(req)?;
    // A relative path is a dead image in a phone's notification shade and a
    // broken one in the WebUI, so it is dropped rather than embedded.
    if !is_absolute_http(&poster) {
        warnings.push(format!(
            "the title's poster is not an absolute http(s) URL and was not attached: {poster}"
        ));
        return None;
    }
    Some(poster)
}

/// The structured enrichment Sonarr's Gotify channel has no room for: Sonarr
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

/// `NotificationMetadataLinkGenerator.GenerateLinks` on Scryer's contract.
///
/// Sonarr iterates the operator's selection and emits a link only when the
/// series carries the id (`Gotify.cs:163-199`), which is preserved here — an
/// unselected site is never linked, and a selected site with no id renders
/// nothing rather than a dead URL. Sonarr's loop has a bug this does not
/// reproduce: when the selected type has no matching id it still appends
/// `[]()`, an empty link line, because `linkText`/`linkUrl` stay empty strings
/// and the `AppendLine` is unconditional.
///
/// The facet decides what "Trakt" and "TMDb" mean, which is the part Sonarr's
/// series-only model cannot express.
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
        // Sonarr's `http://` URLs are emitted as `https://`: every one of these
        // sites redirects, and an http link on a phone is a needless hop.
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

/// `client::notification.click.url` — what tapping the notification opens.
///
/// Sonarr only ever sets the preferred metadata link (`Gotify.cs:195-198`). Two
/// cases are added because the contract carries them and a tap that lands
/// somewhere useful is the whole point of the field: a
/// `ManualInteractionRequired` event carries its own deep link into Scryer and
/// wins, and a test opens the project so the operator can confirm the click
/// target arrived at all.
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
    req.is_test.then(|| SCRYER_LINK.to_string())
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

/// `None` omits the field, which Gotify documents as "the default priority of
/// the application is used".
fn effective_priority(req: &PluginNotificationRequest, settings: &Settings) -> Option<i64> {
    match settings.failure_priority {
        Some(failure)
            if matches!(
                severity(req),
                NotificationSeverity::Warning | NotificationSeverity::Error
            ) =>
        {
            Some(failure)
        }
        _ => settings.priority,
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// One `POST /message` body: `title`, `message`, an optional `priority`, and the
/// `extras` object Sonarr builds in `GotifyMessage.cs:17-34`.
fn build_payload(req: &PluginNotificationRequest, settings: &Settings) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();

    if settings.preferred_metadata_link != PREFERRED_LINK_NONE
        && !settings.metadata_links.is_empty()
        && !settings
            .metadata_links
            .iter()
            .any(|link| link == &settings.preferred_metadata_link)
    {
        warnings.push(format!(
            "preferred_metadata_link '{}' is not among the selected metadata_links; the notification has no click target",
            settings.preferred_metadata_link
        ));
    }

    let links = selected_metadata_links(req, &settings.metadata_links);
    let lines = build_lines(req, settings, &links, &mut warnings);
    let (message, markdown) = render_message(&lines);

    let mut notification = Map::new();
    if let Some(Line::Image(poster)) = lines.iter().find(|line| matches!(line, Line::Image(_))) {
        notification.insert("bigImageUrl".to_string(), json!(poster));
    }
    if let Some(url) = click_url(req, settings, &links) {
        notification.insert("click".to_string(), json!({ "url": url }));
    }

    let mut extras = Map::new();
    extras.insert(
        "client::display".to_string(),
        json!({
            "contentType": if markdown { "text/markdown" } else { "text/plain" },
        }),
    );
    if !notification.is_empty() {
        extras.insert(
            "client::notification".to_string(),
            Value::Object(notification),
        );
    }

    let mut payload = json!({
        "title": heading(req),
        "message": message,
        "extras": Value::Object(extras),
    });
    if let Some(priority) = effective_priority(req, settings) {
        payload["priority"] = Value::from(priority);
    }

    (payload, warnings)
}

fn payload_is_markdown(payload: &Value) -> bool {
    payload
        .get("extras")
        .and_then(|extras| extras.get("client::display"))
        .and_then(|display| display.get("contentType"))
        .and_then(Value::as_str)
        == Some("text/markdown")
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let settings = match Settings::from_config(req.is_test) {
        Ok(settings) => settings,
        Err(error) => return PluginResult::Err(error),
    };

    let (payload, mut warnings) = build_payload(req, &settings);
    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            return PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                "could not encode the Gotify message payload".to_string(),
                Some(error.to_string()),
            ));
        }
    };

    if req.is_test {
        warnings.extend(probe_server(
            &settings.server,
            payload_is_markdown(&payload),
        ));
    }

    // `X-Gotify-Key` rather than Sonarr's `?token=` (`GotifyProxy.cs:28`). Both
    // are still accepted (`appTokenHeader`/`appTokenQuery` in the REST-API
    // spec), the header is what every example in gotify.net/docs/pushmsg uses,
    // and it is the only one of the two that keeps the application token out of
    // reverse-proxy access logs. `appTokenHeader` has existed since gotify
    // v1.0.0, so this needs no version gate.
    let request = HttpRequest::new(format!("{}/message", settings.server))
        .with_method("POST")
        // "Extras […] are only accepted in POST /message requests with
        // application/json content-type" (gotify.net/docs/msgextras).
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_header("X-Gotify-Key", &settings.app_token)
        .with_header("User-Agent", USER_AGENT);

    match http::request::<Vec<u8>>(&request, Some(body)) {
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

/// A Test-time-only `GET /version`.
///
/// `GET /version` carries no security requirement in the REST-API spec, so this
/// costs one unauthenticated round trip and answers two questions Sonarr never
/// asks: is `server` actually a Gotify server, and is it new enough for its
/// WebUI to render the markdown this message is about to use
/// (gotify/server ui v2.0.5, <https://gotify.net/docs/msgextras>)?
///
/// Everything it finds is a warning. A probe that cannot decide must never stop
/// a delivery, and the `POST /message` immediately afterwards produces the real
/// error when the server is genuinely wrong.
fn probe_server(server: &str, markdown: bool) -> Vec<String> {
    let request = HttpRequest::new(format!("{server}/version"))
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);

    let Ok(response) = http::request::<Vec<u8>>(&request, None) else {
        return Vec::new();
    };

    let status = response.status_code();
    if !(200..300).contains(&status) {
        return vec![format!(
            "GET {server}/version answered HTTP {status}: check that server points at a Gotify server"
        )];
    }

    let Some(version) = parse_version(&response.body()) else {
        return vec![format!(
            "GET {server}/version did not answer with Gotify version information: check that server points at a Gotify server"
        )];
    };

    match (markdown, parse_version_triple(&version)) {
        (true, Some(triple)) if triple < MARKDOWN_WEBUI_MIN_VERSION => vec![format!(
            "this Gotify server reports version {version}; its web UI renders markdown only from {}.{}.{}, so the poster and metadata links will show as markdown source there (the Android client renders them from v2.0.7)",
            MARKDOWN_WEBUI_MIN_VERSION.0,
            MARKDOWN_WEBUI_MIN_VERSION.1,
            MARKDOWN_WEBUI_MIN_VERSION.2,
        )],
        _ => Vec::new(),
    }
}

/// `VersionInfo.version` from `GET /version`.
fn parse_version(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("version")?
        .as_str()
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

/// The leading `major.minor.patch` of a Gotify version string.
///
/// `None` for anything that is not a version triple — Gotify's development
/// builds report `unknown` or a branch name, and those must not be treated as
/// "older than 2.0.5".
fn parse_version_triple(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Gotify's `Error` model (`error`, `errorCode`, `errorDescription`) plus the
/// created message's `id`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct GotifyBody {
    error: Option<String>,
    error_code: Option<i64>,
    error_description: Option<String>,
    message_id: Option<i64>,
    /// Whether the body parsed as a JSON object at all. A `false` here is the
    /// single most useful signal this channel has: Gotify answers JSON on every
    /// documented status, so anything else means something that is not Gotify
    /// answered.
    is_json: bool,
    raw: String,
}

impl GotifyBody {
    /// The one line of upstream text quoted back to the operator, bounded: this
    /// ends up in `public_message`, and a server that answers with a whole HTML
    /// page must not turn a notification failure into a wall of markup.
    fn detail(&self, status: u16) -> String {
        for candidate in [self.error_description.as_deref(), self.error.as_deref()] {
            if let Some(text) = candidate.map(str::trim).filter(|text| !text.is_empty()) {
                let text = ellipsize(text, MAX_QUOTED_ERROR);
                return match self.error_code {
                    Some(code) if code as u16 != status => format!("{text} (error {code})"),
                    _ => text,
                };
            }
        }
        match self.raw.trim() {
            "" => format!("HTTP {status}"),
            raw => ellipsize(raw, MAX_QUOTED_ERROR),
        }
    }
}

fn parse_gotify_body(body: &[u8]) -> GotifyBody {
    let raw = String::from_utf8_lossy(body).to_string();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return GotifyBody {
            raw,
            ..GotifyBody::default()
        };
    };
    GotifyBody {
        error: map.get("error").and_then(Value::as_str).map(str::to_string),
        error_code: map.get("errorCode").and_then(Value::as_i64),
        error_description: map
            .get("errorDescription")
            .and_then(Value::as_str)
            .map(str::to_string),
        message_id: map.get("id").and_then(Value::as_i64),
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

/// Sonarr turns every Gotify failure into one of two strings — "Unauthorized -
/// AuthToken is invalid" for a 401 and "Unable to connect to Gotify. Status
/// Code: {0}" for everything else (`GotifyProxy.cs:38-46`) — and neither reaches
/// the operator as anything but a log line. Scryer's typed error lane exists on
/// every send, so the operator is always told which setting to fix.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    mut warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let answer = parse_gotify_body(body);
    let detail = answer.detail(status);
    let debug = format!("HTTP {status}: {detail}");

    if (200..300).contains(&status) {
        let mut response = ok_response();
        response.delivery_id = answer.message_id.map(|id| id.to_string());
        if !answer.is_json {
            // Accepted, but not by something that answered like Gotify. This is
            // a warning rather than a failure: the message may well have been
            // delivered, and refusing a working channel over a proxy's response
            // body would be worse than saying so.
            warnings.push(format!(
                "the server accepted the message with HTTP {status} but did not answer with a Gotify message; check that server points at a Gotify server"
            ));
        }
        response.warnings = warnings;
        return PluginResult::Ok(response);
    }

    // A non-2xx that is not Gotify's documented error JSON did not come from
    // Gotify: an authenticating reverse proxy, a captive portal, or an unrelated
    // service on that origin. Naming `app_token` there would send the operator
    // to the wrong setting, which is exactly what Sonarr's 401 branch does.
    if !answer.is_json && !(500..600).contains(&status) && status != 429 {
        return PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "server did not answer like a Gotify server (HTTP {status}): {detail}. Check the Gotify URL and anything proxying it."
            ),
            Some(debug),
        ));
    }

    match status {
        // "Unauthorized - AuthToken is invalid" (`GotifyProxy.cs:42`). 403 is the
        // same setting from the other side: a token that exists but may not push
        // to this application.
        401 | 403 => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!(
                "app_token was rejected by Gotify (HTTP {status}): {detail}. Gotify 3 shows an application token only when it is created or rotated."
            ),
            Some(debug),
        )),
        // Gotify serves `/message` on every version; a 404 means the base URL is
        // wrong or points at something else.
        404 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "server does not expose Gotify's /message endpoint (HTTP 404): {detail}. Check the Gotify URL, including any path prefix."
            ),
            Some(debug),
        )),
        // The message this plugin built is wrong. The operator has nothing to
        // fix; this is a plugin bug and is reported as one.
        400 => PluginResult::Err(plugin_error(
            PluginErrorCode::Permanent,
            format!("Gotify rejected the message this plugin built: {detail}"),
            Some(debug),
        )),
        // Gotify itself does not rate-limit, but a reverse proxy in front of it
        // does, and `Retry-After` is the one thing the core can act on.
        429 => {
            let mut failure =
                error_response(format!("HTTP 429: {detail}"), Some("http_429".to_string()));
            failure.retry_after_seconds = header(headers, "retry-after")
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|seconds| *seconds >= 0)
                .map(|seconds| seconds.max(1));
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        // The provider saying "not now": the delivery lane, not the
        // configuration lane.
        _ => {
            let mut failure = error_response(
                format!("HTTP {status}: {detail}"),
                Some(format!("http_{status}")),
            );
            failure.retry_after_seconds = header(headers, "retry-after")
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|seconds| *seconds >= 0)
                .map(|seconds| seconds.max(1));
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
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
        PluginNotificationMediaFile, PluginNotificationMediaUpdate, PluginNotificationRelease,
        PluginNotificationTitle,
    };

    fn settings() -> Settings {
        Settings {
            server: "https://gotify.test".to_string(),
            app_token: "apptoken".to_string(),
            priority: Some(5),
            failure_priority: None,
            include_poster: false,
            metadata_links: Vec::new(),
            preferred_metadata_link: "tvdb".to_string(),
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

    fn message_of(payload: &Value) -> &str {
        payload["message"].as_str().expect("a message string")
    }

    fn content_type_of(payload: &Value) -> &str {
        payload["extras"]["client::display"]["contentType"]
            .as_str()
            .expect("a contentType")
    }

    // -----------------------------------------------------------------
    // Descriptor
    // -----------------------------------------------------------------

    #[test]
    fn descriptor_keeps_every_june_config_key_and_fixes_the_field_types() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("gotify must describe a notification provider");
        };

        let by_key = |key: &str| {
            notification
                .config_fields
                .iter()
                .find(|field| field.key == key)
                .unwrap_or_else(|| panic!("{key} must stay a configuration key"))
        };

        for key in [
            "server",
            "app_token",
            "priority",
            "include_series_poster",
            "metadata_links",
            "preferred_metadata_link",
        ] {
            let _ = by_key(key);
        }

        // H1/M2: the three fields whose type was wrong in the June port.
        assert_eq!(by_key("priority").field_type, ConfigFieldType::Select);
        assert_eq!(
            by_key("priority")
                .options
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            vec!["app", "0", "2", "5", "8"],
            "Sonarr's Min/Low/Normal/High values are unchanged"
        );
        assert_eq!(by_key("metadata_links").field_type, ConfigFieldType::Tag);
        assert!(
            by_key("metadata_links")
                .options
                .iter()
                .any(|option| option.value == "imdb")
        );
        assert_eq!(
            by_key("preferred_metadata_link").field_type,
            ConfigFieldType::Select
        );
        assert_eq!(
            by_key("preferred_metadata_link").default_value.as_deref(),
            Some("tvdb"),
            "the stored default is Sonarr's and is a public contract"
        );
        assert_eq!(by_key("app_token").field_type, ConfigFieldType::Password);
    }

    #[test]
    fn descriptor_reports_markdown_and_image_support() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("gotify must describe a notification provider");
        };
        assert!(notification.capabilities.supports_rich_text);
        assert!(notification.capabilities.supports_images);
        assert!(
            notification
                .capabilities
                .payload_formats
                .contains(&NotificationPayloadFormat::Markdown)
        );
        // Self-hosted: there is no vendor origin to allowlist or prefill.
        assert!(notification.allowed_hosts.is_empty());
        assert!(notification.default_base_url.is_none());
    }

    // -----------------------------------------------------------------
    // Settings validation (H1)
    // -----------------------------------------------------------------

    #[test]
    fn a_server_that_is_not_an_absolute_url_names_its_field() {
        let error = normalized_server("gotify.example".to_string()).expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server"));

        let error = normalized_server("https://".to_string()).expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);

        assert_eq!(
            normalized_server("https://gotify.example/".to_string()).unwrap(),
            "https://gotify.example"
        );
        assert_eq!(
            normalized_server("  HTTP://gotify.example  ".to_string()).unwrap(),
            "HTTP://gotify.example",
            "the scheme check is case-insensitive but the value is preserved"
        );
    }

    #[test]
    fn an_unknown_metadata_link_names_its_field() {
        let error = validated_metadata_links(&["imdb".to_string(), "letterboxd".to_string()])
            .expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("metadata_links"));
    }

    #[test]
    fn metadata_links_are_case_insensitive_and_deduplicated() {
        assert_eq!(
            validated_metadata_links(&["IMDb".to_string(), "imdb".to_string(), "TVDB".to_string()])
                .unwrap(),
            vec!["imdb".to_string(), "tvdb".to_string()]
        );
    }

    #[test]
    fn an_unselected_preferred_link_is_strict_at_test_time_and_a_warning_on_a_send() {
        let selected = vec!["imdb".to_string()];

        let error =
            validated_preferred_link(Some("tvdb"), &selected, true).expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("preferred_metadata_link"));

        assert_eq!(
            validated_preferred_link(Some("tvdb"), &selected, false).unwrap(),
            "tvdb",
            "a live send must not lose a notification over a click target"
        );

        let mut settings = settings();
        settings.metadata_links = selected;
        settings.preferred_metadata_link = "tvdb".to_string();
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let (_, warnings) = build_payload(&req, &settings);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("preferred_metadata_link")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_unknown_preferred_link_is_refused_even_on_a_live_send() {
        let error =
            validated_preferred_link(Some("letterboxd"), &[], false).expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("preferred_metadata_link"));
    }

    #[test]
    fn none_is_a_valid_preferred_link_and_suppresses_the_click_target() {
        assert_eq!(
            validated_preferred_link(Some("None"), &["imdb".to_string()], true).unwrap(),
            PREFERRED_LINK_NONE
        );

        let mut settings = settings();
        settings.metadata_links = vec!["tvdb".to_string()];
        settings.preferred_metadata_link = PREFERRED_LINK_NONE.to_string();
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let (payload, warnings) = build_payload(&req, &settings);
        assert!(
            payload["extras"]["client::notification"]
                .get("click")
                .is_none()
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    // -----------------------------------------------------------------
    // Priority (M2)
    // -----------------------------------------------------------------

    #[test]
    fn a_priority_that_is_not_a_number_is_a_typed_error_not_a_silent_default() {
        let parse = |raw: Option<&str>| {
            parse_priority_value("priority", PRIORITY_APPLICATION_DEFAULT, raw, Some(5))
        };

        // The June port used `config_i64("priority", 5)`, which turns every one
        // of these into a silent Normal.
        let error = parse(Some("high")).expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("priority"));

        let error = parse(Some("11")).expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("0 and 10"));

        assert_eq!(
            parse(Some("-1")).expect_err("must be refused").code,
            PluginErrorCode::InvalidConfig
        );

        // Sonarr's four values and Gotify's whole documented range still parse.
        for (raw, expected) in [("0", 0), ("2", 2), ("5", 5), ("8", 8), ("10", 10)] {
            assert_eq!(parse(Some(raw)).unwrap(), Some(expected));
        }

        // Unset keeps the June default rather than silently handing the message
        // to the Gotify application's own default priority, which is 0 — "no
        // notification" — for a newly created application.
        assert_eq!(parse(None).unwrap(), Some(5));
        assert_eq!(parse(Some("  ")).unwrap(), Some(5));

        // The sentinel is the only non-numeric value that is accepted, and it
        // means "send no priority field".
        assert_eq!(parse(Some("app")).unwrap(), None);
        assert_eq!(parse(Some("APP")).unwrap(), None);

        // `failure_priority` shares the rules but defaults to "same as
        // priority", which is also `None`.
        assert_eq!(
            parse_priority_value("failure_priority", FAILURE_PRIORITY_SAME, None, None).unwrap(),
            None
        );
        assert_eq!(
            parse_priority_value(
                "failure_priority",
                FAILURE_PRIORITY_SAME,
                Some("same"),
                None
            )
            .unwrap(),
            None
        );
        assert_eq!(
            parse_priority_value("failure_priority", FAILURE_PRIORITY_SAME, Some("8"), None)
                .unwrap(),
            Some(8)
        );
    }

    #[test]
    fn the_application_default_priority_omits_the_field() {
        let mut settings = settings();
        settings.priority = None;
        let (payload, _) = build_payload(&request(NotificationEventType::Grab), &settings);
        assert!(
            payload.get("priority").is_none(),
            "Gotify uses the application's default priority when none is sent"
        );
    }

    #[test]
    fn a_configured_priority_is_sent_verbatim() {
        let mut settings = settings();
        settings.priority = Some(8);
        let (payload, _) = build_payload(&request(NotificationEventType::Grab), &settings);
        assert_eq!(payload["priority"], json!(8));
    }

    #[test]
    fn failure_priority_only_applies_to_warning_and_error_severities() {
        let mut settings = settings();
        settings.priority = Some(2);
        settings.failure_priority = Some(8);

        let mut info = request(NotificationEventType::Grab);
        info.severity = Some(NotificationSeverity::Info);
        assert_eq!(effective_priority(&info, &settings), Some(2));

        let mut failed = request(NotificationEventType::Download);
        failed.severity = Some(NotificationSeverity::Error);
        assert_eq!(effective_priority(&failed, &settings), Some(8));

        let mut health = request(NotificationEventType::HealthIssue);
        health.severity = Some(NotificationSeverity::Warning);
        assert_eq!(effective_priority(&health, &settings), Some(8));

        settings.failure_priority = None;
        assert_eq!(effective_priority(&failed, &settings), Some(2));
    }

    #[test]
    fn a_missing_severity_falls_back_to_the_dispatchers_own_mapping() {
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

    // -----------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------

    #[test]
    fn the_sparse_request_the_core_sends_today_renders_the_summary_as_plain_text() {
        let (payload, warnings) = build_payload(&request(NotificationEventType::Grab), &settings());
        assert_eq!(payload["title"], json!("Grabbed: Example Show"));
        assert_eq!(
            message_of(&payload),
            "Grabbed 'Example.Show.S01E01' for 'Example Show'."
        );
        assert_eq!(content_type_of(&payload), "text/plain");
        assert!(payload["extras"].get("client::notification").is_none());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_fully_populated_grab_renders_every_structured_field() {
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

        let (payload, _) = build_payload(&req, &settings());
        let message = message_of(&payload);
        for expected in [
            "Episode: 1x01 - Pilot",
            "Quality: WEBDL-1080p",
            "Release: Example.Show.S01E01.1080p.WEB-DL",
            "Release Group: GROUP",
            "Indexer: Example Indexer",
            "Size: 2 GB",
            "Client: Weaver",
        ] {
            assert!(
                message.contains(expected),
                "{expected:?} missing: {message}"
            );
        }
    }

    #[test]
    fn every_event_type_renders_without_panicking_and_never_empty() {
        for event_type in general_notification_events() {
            let mut req = request(event_type);
            req.summary_message = String::new();
            req.summary_title = String::new();
            let (payload, _) = build_payload(&req, &settings());
            assert!(
                !message_of(&payload).is_empty(),
                "{event_type:?} produced an empty message, which Gotify rejects"
            );
        }
    }

    #[test]
    fn the_download_event_renders_a_failure_not_an_import() {
        let mut req = request(NotificationEventType::Download);
        req.summary_message = "Download failed: Example.Show.S01E01".to_string();
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            status: Some("failed".to_string()),
            status_message: Some("all articles missing".to_string()),
            ..PluginNotificationDownload::default()
        });
        let (payload, _) = build_payload(&req, &settings());
        let message = message_of(&payload);
        assert!(message.contains("Download failed"));
        assert!(message.contains("Status: all articles missing"));
        assert!(!message.contains("Destination"));
    }

    #[test]
    fn import_rename_delete_health_update_and_manual_events_render_their_blocks() {
        let mut import = request(NotificationEventType::ImportComplete);
        import.import = Some(PluginNotificationImport {
            dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            ..PluginNotificationImport::default()
        });
        assert!(message_of(&build_payload(&import, &settings()).0).contains("Destination: /media"));

        let mut rename = request(NotificationEventType::Rename);
        rename.file = Some(PluginNotificationFile {
            primary_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            media_updates: Vec::new(),
        });
        assert!(message_of(&build_payload(&rename, &settings()).0).contains("File: /media"));

        let mut deleted = request(NotificationEventType::FileDeleted);
        deleted.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Example Show/old.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });
        assert!(message_of(&build_payload(&deleted, &settings()).0).contains("old.mkv"));

        let mut health = request(NotificationEventType::HealthIssue);
        health.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable".to_string()),
            ..PluginNotificationHealth::default()
        });
        let message = message_of(&build_payload(&health, &settings()).0).to_string();
        assert!(message.contains("Check: IndexerStatusCheck"));
        assert!(message.contains("Detail: Indexers unavailable"));

        let mut update = request(NotificationEventType::ApplicationUpdate);
        update.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            ..PluginNotificationApplicationUpdate::default()
        });
        let message = message_of(&build_payload(&update, &settings()).0).to_string();
        assert!(message.contains("Previous Version: 0.19.7"));
        assert!(message.contains("New Version: 0.19.8"));

        let mut manual = request(NotificationEventType::ManualInteractionRequired);
        manual.manual_interaction = Some(PluginNotificationManualInteraction {
            reason: Some("Import needs a decision".to_string()),
            link: Some("https://scryer.test/queue".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        let (payload, _) = build_payload(&manual, &settings());
        assert!(message_of(&payload).contains("Reason: Import needs a decision"));
        assert_eq!(
            payload["extras"]["client::notification"]["click"]["url"],
            json!("https://scryer.test/queue"),
            "a manual-interaction deep link is the most useful tap target there is"
        );

        let mut subtitles = request(NotificationEventType::SubtitleDownloaded);
        subtitles.media_files = vec![PluginNotificationMediaFile {
            path: "/media/TV/Example Show/S01E01.mkv".to_string(),
            subtitle_languages: vec!["English".to_string(), "Dutch".to_string()],
            ..PluginNotificationMediaFile::default()
        }];
        assert!(
            message_of(&build_payload(&subtitles, &settings()).0)
                .contains("Languages: English, Dutch")
        );
    }

    // -----------------------------------------------------------------
    // Markdown (L1) and extras
    // -----------------------------------------------------------------

    #[test]
    fn markdown_significant_characters_are_escaped_once_the_message_is_markdown() {
        let mut settings = settings();
        settings.include_poster = true;

        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.summary_message = "Grabbed 'Show.S01E01.*RAW*_v2_[uncut]' for 'Show'".to_string();

        let (payload, _) = build_payload(&req, &settings);
        assert_eq!(content_type_of(&payload), "text/markdown");
        let message = message_of(&payload);
        assert!(
            message.contains(r"Show.S01E01.\*RAW\*\_v2\_\[uncut\]"),
            "an unescaped * or _ renders as emphasis and eats the characters: {message}"
        );
        // The image line this plugin wrote must stay live markdown.
        assert!(message.contains("![](https://images.test/poster.jpg)"));
    }

    #[test]
    fn a_plain_text_message_is_never_escaped() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_message = "Grabbed 'Show.S01E01.*RAW*'".to_string();
        let (payload, _) = build_payload(&req, &settings());
        assert_eq!(content_type_of(&payload), "text/plain");
        assert!(message_of(&payload).contains("*RAW*"));
    }

    #[test]
    fn the_title_is_not_escaped_because_gotify_renders_it_as_plain_text() {
        let mut settings = settings();
        settings.include_poster = true;
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.summary_title = "Grabbed: *Example* Show".to_string();
        let (payload, _) = build_payload(&req, &settings);
        assert_eq!(content_type_of(&payload), "text/markdown");
        assert_eq!(payload["title"], json!("Grabbed: *Example* Show"));
    }

    #[test]
    fn a_poster_becomes_both_a_markdown_image_and_the_big_notification_image() {
        let mut settings = settings();
        settings.include_poster = true;
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let (payload, warnings) = build_payload(&req, &settings);
        assert_eq!(
            payload["extras"]["client::notification"]["bigImageUrl"],
            json!("https://images.test/poster.jpg")
        );
        assert!(message_of(&payload).contains("![](https://images.test/poster.jpg)"));
        assert_eq!(content_type_of(&payload), "text/markdown");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_relative_poster_is_dropped_with_a_warning_rather_than_embedded_dead() {
        let mut settings = settings();
        settings.include_poster = true;
        let mut title = series_title();
        title.poster_url = Some("/media/poster.jpg".to_string());
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(title);

        let (payload, warnings) = build_payload(&req, &settings);
        assert!(payload["extras"].get("client::notification").is_none());
        assert_eq!(content_type_of(&payload), "text/plain");
        assert!(
            warnings.iter().any(|warning| warning.contains("poster")),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_poster_toggle_off_sends_no_image_at_all() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let (payload, _) = build_payload(&req, &settings());
        assert!(payload["extras"].get("client::notification").is_none());
        assert_eq!(content_type_of(&payload), "text/plain");
    }

    #[test]
    fn extras_use_gotifys_double_colon_namespaces() {
        let mut settings = settings();
        settings.include_poster = true;
        settings.metadata_links = vec!["tvdb".to_string()];
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let (payload, _) = build_payload(&req, &settings);
        let extras = payload["extras"].as_object().expect("an extras object");
        assert!(extras.contains_key("client::display"));
        assert!(extras.contains_key("client::notification"));
        assert!(
            payload["extras"]["client::notification"]["click"]["url"]
                .as_str()
                .is_some()
        );
    }

    // -----------------------------------------------------------------
    // Metadata links (M1)
    // -----------------------------------------------------------------

    #[test]
    fn series_links_use_the_series_ids_and_render_in_the_selected_order() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let links = selected_metadata_links(
            &req,
            &[
                "tvdb".to_string(),
                "imdb".to_string(),
                "trakt".to_string(),
                "tvmaze".to_string(),
            ],
        );
        assert_eq!(
            links
                .iter()
                .map(|(_, label, url)| (*label, url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("TVDb", "https://thetvdb.com/?tab=series&id=12345"),
                ("IMDb", "https://www.imdb.com/title/tt0903747"),
                ("Trakt", "https://trakt.tv/search/tvdb/12345?id_type=show"),
                ("TVMaze", "https://www.tvmaze.com/shows/82"),
            ]
        );
    }

    #[test]
    fn movie_links_use_tmdb_and_imdb_which_sonarrs_series_only_model_cannot_express() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(movie_title());
        let links = selected_metadata_links(&req, &["tmdb".to_string(), "trakt".to_string()]);
        assert_eq!(
            links
                .iter()
                .map(|(_, label, url)| (*label, url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("TMDb", "https://www.themoviedb.org/movie/603"),
                ("Trakt", "https://trakt.tv/search/tmdb/603?id_type=movie"),
            ]
        );
    }

    #[test]
    fn anime_ids_come_from_the_typed_fields_or_the_by_source_map() {
        let mut title = series_title();
        title.facet = "anime".to_string();
        title.external_ids.anidb_id = Some("979".to_string());
        title
            .external_ids
            .by_source
            .insert("kitsu".to_string(), vec!["1376".to_string()]);
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(title);

        let links = selected_metadata_links(
            &req,
            &["anidb".to_string(), "kitsu".to_string(), "mal".to_string()],
        );
        assert_eq!(
            links
                .iter()
                .map(|(_, label, url)| (*label, url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("AniDB", "https://anidb.net/anime/979"),
                ("Kitsu", "https://kitsu.app/anime/1376"),
            ],
            "a selected site with no id renders nothing rather than a dead link"
        );
    }

    #[test]
    fn a_selected_site_with_no_id_renders_no_empty_link_line() {
        let mut title = series_title();
        title.external_ids.imdb_id = None;
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(title);

        let mut settings = settings();
        settings.metadata_links = vec!["imdb".to_string(), "tvdb".to_string()];
        let (payload, _) = build_payload(&req, &settings);
        let message = message_of(&payload);
        assert!(
            !message.contains("[]()"),
            "Sonarr appends an empty link line here (Gotify.cs:193): {message}"
        );
        assert!(message.contains("[TVDb](https://thetvdb.com/?tab=series&id=12345)"));
    }

    #[test]
    fn a_request_with_no_title_renders_no_links() {
        let req = request(NotificationEventType::HealthIssue);
        assert!(selected_metadata_links(&req, &["tvdb".to_string()]).is_empty());
    }

    #[test]
    fn the_preferred_link_becomes_the_click_url() {
        let mut settings = settings();
        settings.metadata_links = vec!["tvdb".to_string(), "imdb".to_string()];
        settings.preferred_metadata_link = "imdb".to_string();
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let (payload, _) = build_payload(&req, &settings);
        assert_eq!(
            payload["extras"]["client::notification"]["click"]["url"],
            json!("https://www.imdb.com/title/tt0903747")
        );
    }

    #[test]
    fn a_test_carries_a_link_and_a_click_target_the_way_sonarrs_does() {
        let (payload, _) = build_payload(&request(NotificationEventType::Test), &settings());
        assert_eq!(content_type_of(&payload), "text/markdown");
        assert!(message_of(&payload).contains(&format!("[Scryer]({SCRYER_LINK})")));
        assert_eq!(
            payload["extras"]["client::notification"]["click"]["url"],
            json!(SCRYER_LINK)
        );
    }

    #[test]
    fn a_link_destination_is_percent_escaped_not_backslash_escaped() {
        let line = Line::Link {
            label: "A (B)".to_string(),
            url: "https://example.test/a b(c)".to_string(),
        };
        assert_eq!(
            line.render(true),
            "[A (B)](https://example.test/a%20b%28c%29)"
        );
    }

    // -----------------------------------------------------------------
    // Delivery classification (H2)
    // -----------------------------------------------------------------

    fn classify(status: u16, body: &str) -> PluginResult<PluginNotificationResponse> {
        classify_response(status, &BTreeMap::new(), body.as_bytes(), Vec::new())
    }

    fn expect_error(result: PluginResult<PluginNotificationResponse>) -> PluginError {
        match result {
            PluginResult::Err(error) => error,
            other => panic!("expected a typed plugin error, got {other:?}"),
        }
    }

    fn expect_response(
        result: PluginResult<PluginNotificationResponse>,
    ) -> PluginNotificationResponse {
        match result {
            PluginResult::Ok(response) => response,
            other => panic!("expected a delivery response, got {other:?}"),
        }
    }

    #[test]
    fn a_created_message_reports_its_id_as_the_delivery_id() {
        let response = expect_response(classify(200, r#"{"id":42,"appid":5,"message":"x"}"#));
        assert!(response.success);
        assert_eq!(response.delivery_id.as_deref(), Some("42"));
        assert!(response.warnings.is_empty());
    }

    #[test]
    fn a_two_hundred_that_is_not_gotify_json_still_succeeds_but_warns() {
        let response = expect_response(classify(200, "<html>ok</html>"));
        assert!(response.success);
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("server")),
            "{:?}",
            response.warnings
        );
    }

    #[test]
    fn a_rejected_token_is_auth_failed_naming_app_token() {
        for status in [401u16, 403] {
            let error = expect_error(classify(
                status,
                r#"{"error":"Unauthorized","errorCode":401,"errorDescription":"you need to provide a valid access token or user credentials to access this api"}"#,
            ));
            assert_eq!(error.code, PluginErrorCode::AuthFailed);
            assert!(error.public_message.contains("app_token"));
            assert!(error.public_message.contains("valid access token"));
        }
    }

    #[test]
    fn a_404_is_invalid_config_naming_server() {
        let error = expect_error(classify(
            404,
            r#"{"error":"Not Found","errorCode":404,"errorDescription":"page not found"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server"));
    }

    #[test]
    fn a_non_json_failure_body_is_invalid_config_naming_server_not_the_token() {
        // An authenticating reverse proxy in front of Gotify answers 401 with an
        // HTML login page. Sonarr blames the token here.
        let error = expect_error(classify(401, "<html><body>Sign in</body></html>"));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server"));
        assert!(!error.public_message.contains("app_token"));
    }

    #[test]
    fn a_400_quotes_gotifys_error_description_as_a_permanent_plugin_fault() {
        let error = expect_error(classify(
            400,
            r#"{"error":"Bad Request","errorCode":400,"errorDescription":"json: cannot unmarshal number into Go struct field"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("cannot unmarshal"));
    }

    #[test]
    fn a_5xx_is_a_delivery_failure_carrying_the_provider_status() {
        let response = expect_response(classify(503, r#"{"error":"unavailable"}"#));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_503"));
        assert!(response.error.as_deref().unwrap().contains("unavailable"));
    }

    #[test]
    fn a_non_json_5xx_stays_in_the_delivery_lane() {
        let response = expect_response(classify(502, "<html>bad gateway</html>"));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_502"));
    }

    #[test]
    fn a_429_carries_retry_after_seconds_from_the_header() {
        let result = classify_response(
            429,
            &headers(&[("Retry-After", "30")]),
            b"rate limited",
            Vec::new(),
        );
        let response = expect_response(result);
        assert!(!response.success);
        assert_eq!(response.retry_after_seconds, Some(30));
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));
    }

    #[test]
    fn warnings_survive_every_classification_lane() {
        let warning = vec!["poster dropped".to_string()];
        let ok = expect_response(classify_response(
            200,
            &BTreeMap::new(),
            br#"{"id":1}"#,
            warning.clone(),
        ));
        assert_eq!(ok.warnings, warning);

        let failed = expect_response(classify_response(
            500,
            &BTreeMap::new(),
            b"{}",
            warning.clone(),
        ));
        assert_eq!(failed.warnings, warning);
    }

    #[test]
    fn an_error_body_is_quoted_but_bounded() {
        let long = "x".repeat(1000);
        let error = expect_error(classify(404, &format!(r#"{{"error":"{long}"}}"#)));
        assert!(error.public_message.chars().count() < 500);
    }

    // -----------------------------------------------------------------
    // Version probe
    // -----------------------------------------------------------------

    #[test]
    fn a_gotify_version_body_parses_and_a_development_build_does_not_gate() {
        assert_eq!(
            parse_version(br#"{"version":"2.6.3","commit":"abc","buildDate":"x"}"#).as_deref(),
            Some("2.6.3")
        );
        assert_eq!(parse_version(b"<html>"), None);
        assert_eq!(parse_version(br#"{"name":"something else"}"#), None);

        assert_eq!(parse_version_triple("v3.1.0"), Some((3, 1, 0)));
        assert_eq!(parse_version_triple("2.0.5-rc.1"), Some((2, 0, 5)));
        assert_eq!(parse_version_triple("2.1"), Some((2, 1, 0)));
        assert_eq!(
            parse_version_triple("unknown"),
            None,
            "a development build must not be read as older than 2.0.5"
        );
    }

    #[test]
    fn the_markdown_gate_is_the_documented_web_ui_floor() {
        assert!(parse_version_triple("2.0.4").unwrap() < MARKDOWN_WEBUI_MIN_VERSION);
        assert!(parse_version_triple("2.0.5").unwrap() >= MARKDOWN_WEBUI_MIN_VERSION);
        assert!(parse_version_triple("3.1.0").unwrap() >= MARKDOWN_WEBUI_MIN_VERSION);
    }

    // -----------------------------------------------------------------
    // Misc
    // -----------------------------------------------------------------

    #[test]
    fn sizes_round_the_way_sonarr_rounds_them() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
        assert_eq!(format_bytes(1_536), "1.5 KB");
    }

    #[test]
    fn payload_is_markdown_reads_the_extras_the_plugin_wrote() {
        let mut with_poster = settings();
        with_poster.include_poster = true;
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let (markdown_payload, _) = build_payload(&req, &with_poster);
        assert!(payload_is_markdown(&markdown_payload));

        let (plain_payload, _) = build_payload(&request(NotificationEventType::Grab), &settings());
        assert!(!payload_is_markdown(&plain_payload));
    }
}
