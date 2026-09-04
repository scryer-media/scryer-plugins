//! Telegram Bot API notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Telegram notification (`src/NzbDrone.Core/Notifications/Telegram/`)
//! is a small channel with three parts that matter: a per-event title, an
//! operator-selected set of metadata links appended as `<a href>` lines, and a
//! `link_preview_options` object driven by a second setting that picks *which*
//! of those links Telegram should preview. The June port had none of them — it
//! sent a bold `summary_title`, the `summary_message`, and `is_disabled: true`
//! on every message.
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * `metadata_links` (a `Tag` field) and `link_preview` (a `Select`) carry
//!   Sonarr's two settings, generalised from Sonarr's series-only world to
//!   Scryer's facets — TVDb/TVMaze/Trakt for series, TMDb/IMDb for movies, and
//!   the anime id set when the contract carries one;
//! * the body is enriched per event from the structured blocks the contract
//!   carries (episode, quality, release, indexer, client, paths, health,
//!   version) instead of being `summary_message` alone;
//! * Telegram's own error JSON is parsed and mapped to the offending
//!   **configuration field**, which is what Sonarr does only inside `Test`
//!   (`TelegramProxy.cs:72-119`) and what Scryer's typed `PluginError` lane
//!   lets this channel do on every send;
//! * the message is measured in *rendered* characters — Bot API `sendMessage`
//!   documents `text` as "1-4096 characters after entities parsing" — and
//!   trimmed to fit with a `warnings` entry rather than being rejected by
//!   Telegram.
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")`. Telegram's failures are not one shape: a `401` is the bot
//! token, a `400` carrying `"chat not found"` is the chat id, a `400` carrying
//! `"message thread not found"` is the topic id, a `429` carries
//! `parameters.retry_after` the core can act on, and a `400` carrying
//! `"can't parse entities"` is a bug in the markup *this plugin* built. Those
//! are three different lanes in Scryer's contract, so the send lives here.
//!
//! # Upstream reference
//!
//! Bot API 10.3 (24 August 2026), <https://core.telegram.org/bots/api>:
//! `sendMessage`, `LinkPreviewOptions`, `ResponseParameters`, and the
//! "Formatting options → HTML style" section.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, PluginNotificationEpisode,
    current_sdk_constraint,
};
use serde_json::{Value, json};

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

const PROVIDER_TYPE: &str = "telegram";
const USER_AGENT: &str = concat!("scryer-telegram-plugin/", env!("CARGO_PKG_VERSION"));

const TELEGRAM_API_URL: &str = "https://api.telegram.org";
const TELEGRAM_API_HOST: &str = "api.telegram.org";

/// `sendMessage` → `text`: "1-4096 characters **after entities parsing**"
/// (core.telegram.org/bots/api#sendmessage). The cap is on the message Telegram
/// renders, so the markup this module adds does not count against it — which is
/// why every line below is measured by its visible text and only escaped on the
/// way out.
const MESSAGE_CHARACTER_LIMIT: usize = 4096;

/// A line shorter than this cannot carry a useful truncated value, so it is
/// dropped instead of being reduced to a label and an ellipsis.
const MIN_TRUNCATED_LINE: usize = 8;

/// The link shown on a test message, standing in for Sonarr's `sonarr.tv`
/// (`TelegramProxy.cs:80-83`). Sonarr sends a link on the test so the operator
/// can see that link rendering works at all.
const SCRYER_LINK: &str = "https://github.com/scryer-media/scryer";

// ---------------------------------------------------------------------------
// Metadata links (MetadataLinkType.cs, NotificationMetadataLinkGenerator.cs)
// ---------------------------------------------------------------------------

/// Sonarr's four options plus the ones its series-only world cannot express.
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

/// `MetadataLinkPreviewType` (`MetadataLinkType.cs` in the Telegram folder).
///
/// TVDb is absent for the reason Sonarr commented it out: thetvdb.com serves no
/// preview data, so selecting it would silently produce a message with no
/// preview at all.
const LINK_PREVIEW_OPTIONS: &[(&str, &str)] = &[
    ("none", "None"),
    ("imdb", "IMDb"),
    ("tvmaze", "TVMaze"),
    ("trakt", "Trakt"),
    ("tmdb", "TMDb"),
    ("anidb", "AniDB"),
    ("anilist", "AniList"),
    ("mal", "MyAnimeList"),
    ("kitsu", "Kitsu"),
];

const LINK_PREVIEW_NONE: &str = "none";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Telegram".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // Fixed by the product: every bot request is
            // `https://api.telegram.org/bot<token>/METHOD`. There is no
            // operator-supplied base URL, so this is documentation rather than
            // a prefill (nothing auto-provisions a notification channel).
            default_base_url: Some(TELEGRAM_API_URL.to_string()),
            allowed_hosts: vec![TELEGRAM_API_HOST.to_string()],
            capabilities: NotificationCapabilities {
                supports_rich_text: true,
                // No `sendPhoto` path: a photo message replaces the text with a
                // 1024-character caption, which is a strictly worse message.
                supports_images: false,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![
                    NotificationDeliveryMode::Chat,
                    NotificationDeliveryMode::Push,
                ],
                payload_formats: vec![
                    NotificationPayloadFormat::PlainText,
                    NotificationPayloadFormat::Html,
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
        field(
            "bot_token",
            "Bot Token",
            ConfigFieldType::Password,
            true,
            None,
            Some("The token @BotFather issued for this bot."),
        ),
        field(
            "chat_id",
            "Chat ID",
            ConfigFieldType::String,
            true,
            None,
            Some(
                "Numeric chat, group or channel id, or an @username for a public channel. The bot must be a member of the chat.",
            ),
        ),
        field(
            "topic_id",
            "Topic ID",
            ConfigFieldType::Number,
            false,
            None,
            Some(
                "Forum-topic (message thread) id. Must be greater than 1, or empty for the General topic.",
            ),
        ),
        field(
            "send_silently",
            "Send Silently",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Delivers the message with no notification sound."),
        ),
        field(
            "include_app_name_in_title",
            "Include App Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Prefixes the heading with the Scryer application name."),
        ),
        field(
            "include_instance_name_in_title",
            "Include Instance Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "No effect: Scryer's notification contract carries no instance name. Kept so existing configurations keep parsing.",
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
            "link_preview",
            "Link Preview",
            Some(LINK_PREVIEW_NONE),
            LINK_PREVIEW_OPTIONS,
        ),
    ]
}

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

/// Everything the renderer needs from configuration, resolved and validated
/// once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    bot_token: String,
    chat_id: String,
    topic_id: Option<i64>,
    send_silently: bool,
    include_app_name_in_title: bool,
    metadata_links: Vec<String>,
    link_preview: String,
}

impl Settings {
    /// `strict` is the Test-time posture: a `link_preview` that names an
    /// unselected site is refused so the operator sees it in the connection
    /// test. On a live send the same mismatch degrades to "no preview" with a
    /// warning instead — a preview setting must never stop notifications.
    fn from_config(strict: bool) -> Result<Self, PluginError> {
        let bot_token = required_config("bot_token").map_err(config_error)?;
        let chat_id = required_config("chat_id").map_err(config_error)?;
        let topic_id = parse_topic_id(config_value("topic_id").as_deref())?;
        let metadata_links = validated_metadata_links(&config_csv("metadata_links"))?;
        let link_preview = validated_link_preview(
            config_value("link_preview").as_deref(),
            &metadata_links,
            strict,
        )?;

        Ok(Self {
            bot_token,
            chat_id,
            topic_id,
            send_silently: config_bool("send_silently"),
            include_app_name_in_title: config_bool("include_app_name_in_title"),
            metadata_links,
            link_preview,
        })
    }
}

/// `TelegramSettingsValidator` (`TelegramSettings.cs:15-16`): "Topic ID must be
/// greater than 1 or empty".
///
/// Sonarr can only say this at settings-validation time; the June port answered
/// it as a *delivery failure*, which told the operator a message failed to send
/// rather than that a setting is wrong. It is a typed `InvalidConfig` naming the
/// field.
fn parse_topic_id(raw: Option<&str>) -> Result<Option<i64>, PluginError> {
    let Some(raw) = raw else { return Ok(None) };
    match raw.parse::<i64>() {
        Ok(topic_id) if topic_id > 1 => Ok(Some(topic_id)),
        Ok(topic_id) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "topic_id must be greater than 1, or empty for the General topic".to_string(),
            Some(format!("configured value: {topic_id}")),
        )),
        Err(error) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "topic_id must be a whole number".to_string(),
            Some(format!("configured value {raw:?}: {error}")),
        )),
    }
}

/// `TelegramSettings.cs:17-26`: every selected link must be a known option.
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

/// `TelegramSettings.cs:28-40`: the preview value must be a known option and —
/// unless it is `None` — one of the selected metadata links, because a preview
/// URL Telegram never receives is a silently dead setting. The second rule is
/// enforced only when `strict` (the connection test); a live send keeps
/// delivering and reports the mismatch as a warning (`build_payload`).
fn validated_link_preview(
    raw: Option<&str>,
    metadata_links: &[String],
    strict: bool,
) -> Result<String, PluginError> {
    let value = raw
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| LINK_PREVIEW_NONE.to_string());

    if !LINK_PREVIEW_OPTIONS.iter().any(|(key, _)| *key == value) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("link_preview is not a valid value: {value}"),
            Some(format!(
                "known values: {}",
                option_keys(LINK_PREVIEW_OPTIONS)
            )),
        ));
    }

    if strict && value != LINK_PREVIEW_NONE && !metadata_links.iter().any(|link| link == &value) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "link_preview must be one of the selected metadata_links; {value} is not selected"
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
// The Bot API caps `text` at 4096 characters *after entities parsing*, so the
// message is built as typed lines whose visible length is known before any
// escaping or tag wrapping happens. Truncating the rendered HTML instead would
// both mis-count (`&amp;` is one rendered character) and risk cutting a tag in
// half, which Telegram answers with `400 can't parse entities`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// `<b>text</b>` — the message heading.
    Heading(String),
    /// `<b>Label:</b> value`
    Labeled(&'static str, String),
    Plain(String),
    Link {
        label: String,
        url: String,
    },
}

impl Line {
    fn visible_len(&self) -> usize {
        match self {
            Line::Heading(text) | Line::Plain(text) => char_count(text),
            Line::Labeled(label, value) => char_count(label) + 2 + char_count(value),
            Line::Link { label, .. } => char_count(label),
        }
    }

    fn render(&self) -> String {
        match self {
            Line::Heading(text) => format!("<b>{}</b>", html_escape(text)),
            Line::Labeled(label, value) => {
                format!("<b>{}:</b> {}", html_escape(label), html_escape(value))
            }
            Line::Plain(text) => html_escape(text),
            // Sonarr writes the href raw (`TelegramProxy.cs:47`), which leaves
            // the bare `&` of its own TVDb URL inside an HTML attribute.
            // Telegram's HTML mode requires `<`, `>` and `&` to be entities
            // everywhere in the text.
            Line::Link { label, url } => format!(
                "<a href=\"{}\">{}</a>",
                html_escape(url),
                html_escape(label)
            ),
        }
    }

    /// The same line reduced to `budget` visible characters, or `None` when the
    /// budget cannot hold anything worth sending.
    fn truncated_to(&self, budget: usize) -> Option<Line> {
        if budget < MIN_TRUNCATED_LINE {
            return None;
        }
        match self {
            Line::Heading(text) => Some(Line::Heading(ellipsize(text, budget))),
            Line::Plain(text) => Some(Line::Plain(ellipsize(text, budget))),
            Line::Labeled(label, value) => {
                let room = budget.checked_sub(char_count(label) + 2)?;
                (room >= 4).then(|| Line::Labeled(label, ellipsize(value, room)))
            }
            // A link with a cut-off label is noise, and the href is not
            // shortenable at all.
            Line::Link { .. } => None,
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
    let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Render the lines into `text`, dropping the tail that does not fit.
///
/// The heading and the summary come first and the enrichment and links last, so
/// trimming from the end degrades detail rather than meaning.
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
            "message trimmed to Telegram's {MESSAGE_CHARACTER_LIMIT}-character limit{detail}"
        ));
        break;
    }

    (rendered.join("\n"), warnings)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// `Telegram.cs:25-110` builds a fixed per-event title and hands the event's own
/// message straight to the proxy. Scryer's dispatcher already puts an
/// event-specific heading in `summary_title` ("Grabbed: X", "Import complete:
/// X", "Download failed: X"), so that is the heading; the branding toggle is the
/// only thing Sonarr adds that the contract does not.
fn heading(req: &PluginNotificationRequest, settings: &Settings) -> String {
    let title = req.summary_title.trim();
    let title = if title.is_empty() {
        req.app.name.trim()
    } else {
        title
    };
    if settings.include_app_name_in_title {
        format!("{} - {title}", req.app.name.trim())
    } else {
        title.to_string()
    }
}

fn build_lines(req: &PluginNotificationRequest, settings: &Settings) -> Vec<Line> {
    let mut lines = vec![Line::Heading(heading(req, settings))];

    let message = req.summary_message.trim();
    if !message.is_empty() {
        lines.push(Line::Plain(message.to_string()));
    }

    lines.extend(detail_lines(req));

    if req.is_test {
        lines.push(Line::Link {
            label: req.app.name.trim().to_string(),
            url: SCRYER_LINK.to_string(),
        });
    }

    for (_, label, url) in selected_metadata_links(req, &settings.metadata_links) {
        lines.push(Line::Link {
            label: label.to_string(),
            url,
        });
    }

    lines
}

/// The structured enrichment Sonarr's Telegram channel has no room for: Sonarr
/// sends one prose sentence per event, while Scryer's contract carries the facts
/// separately. Every line is conditional on the block actually being present, so
/// the sparse shape the core sends today renders exactly the two lines the June
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
            if let Some(link) = manual_link(req) {
                lines.push(Line::Link {
                    label: "Open in Scryer".to_string(),
                    url: link,
                });
            }
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

/// Only an absolute http(s) link is rendered: an `<a href>` Telegram cannot
/// resolve is a dead entity in the message.
fn manual_link(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.manual_interaction
            .as_ref()
            .and_then(|interaction| interaction.link.clone()),
    )
    .filter(|link| {
        let link = link.to_ascii_lowercase();
        link.starts_with("http://") || link.starts_with("https://")
    })
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
/// series carries the id, which is preserved here — an unselected site is never
/// linked, and a selected site with no id renders nothing rather than a dead
/// URL. The facet decides what "Trakt" and "TMDb" mean, which is the part
/// Sonarr's series-only model cannot express.
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
        // sites redirects, and an http link in a message is a needless hop.
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

/// `TelegramLinkPreviewOptions` (`TelegramLinkPreviewOptions.cs:15-19`).
///
/// One deliberate difference: Sonarr sets `is_disabled: false` with a null `url`
/// when the chosen site has no id for this title, which makes Telegram preview
/// whatever URL it finds first in the text. Here an unavailable preview target
/// disables the preview, so the operator's choice is either honoured or absent —
/// never silently replaced by a different site.
fn link_preview_options(settings: &Settings, links: &[(String, &'static str, String)]) -> Value {
    if settings.link_preview == LINK_PREVIEW_NONE {
        return json!({ "is_disabled": true });
    }
    match links
        .iter()
        .find(|(key, _, _)| key == &settings.link_preview)
    {
        Some((_, _, url)) => json!({ "is_disabled": false, "url": url }),
        None => json!({ "is_disabled": true }),
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

fn build_payload(req: &PluginNotificationRequest, settings: &Settings) -> (Value, Vec<String>) {
    let lines = build_lines(req, settings);
    let (text, mut warnings) = render_message(&lines);

    if settings.link_preview != LINK_PREVIEW_NONE
        && !settings
            .metadata_links
            .iter()
            .any(|link| link == &settings.link_preview)
    {
        warnings.push(format!(
            "link_preview '{}' is not among the selected metadata_links; preview disabled",
            settings.link_preview
        ));
    }

    let mut payload = json!({
        "chat_id": settings.chat_id,
        "parse_mode": "HTML",
        "text": text,
        "disable_notification": settings.send_silently,
        // `disable_web_page_preview` was replaced by `link_preview_options` in
        // Bot API 7.0 (29 December 2023) and no longer appears in the reference.
        "link_preview_options": link_preview_options(
            settings,
            &selected_metadata_links(req, &settings.metadata_links),
        ),
    });
    if let Some(topic_id) = settings.topic_id {
        payload["message_thread_id"] = Value::from(topic_id);
    }
    (payload, warnings)
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let settings = match Settings::from_config(req.is_test) {
        Ok(settings) => settings,
        Err(error) => return PluginResult::Err(error),
    };

    let (payload, warnings) = build_payload(req, &settings);
    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            return PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                "could not encode the Telegram sendMessage payload".to_string(),
                Some(error.to_string()),
            ));
        }
    };

    // Bot API method names are case-insensitive ("All methods in the Bot API are
    // case-insensitive"); `sendmessage` is the spelling Sonarr uses
    // (`TelegramProxy.cs:50`) and the one this crate's host-conformance suite
    // pins.
    let url = format!(
        "{TELEGRAM_API_URL}/bot{}/sendmessage",
        settings.bot_token.trim()
    );
    let request = HttpRequest::new(&url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
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

/// The response body every Bot API call returns
/// (core.telegram.org/bots/api#making-requests, `ResponseParameters`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TelegramBody {
    ok: Option<bool>,
    error_code: Option<i64>,
    description: Option<String>,
    retry_after: Option<i64>,
    migrate_to_chat_id: Option<i64>,
    message_id: Option<i64>,
}

impl TelegramBody {
    fn detail(&self, status: u16) -> String {
        match self.description.as_deref().map(str::trim) {
            Some(description) if !description.is_empty() => match self.error_code {
                Some(code) if code as u16 != status => format!("{description} (error {code})"),
                _ => description.to_string(),
            },
            _ => format!("HTTP {status}"),
        }
    }

    fn describes(&self, needle: &str) -> bool {
        self.description
            .as_deref()
            .map(|description| description.to_ascii_lowercase().contains(needle))
            .unwrap_or(false)
    }
}

fn parse_telegram_body(body: &[u8]) -> TelegramBody {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return TelegramBody {
            description: non_empty(Some(String::from_utf8_lossy(body).to_string()))
                .map(|text| ellipsize(&text, 500)),
            ..TelegramBody::default()
        };
    };
    let parameters = map.get("parameters").and_then(Value::as_object);
    TelegramBody {
        ok: map.get("ok").and_then(Value::as_bool),
        error_code: map.get("error_code").and_then(Value::as_i64),
        description: map
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        retry_after: parameters
            .and_then(|parameters| parameters.get("retry_after"))
            .and_then(Value::as_i64),
        migrate_to_chat_id: parameters
            .and_then(|parameters| parameters.get("migrate_to_chat_id"))
            .and_then(Value::as_i64),
        message_id: map
            .get("result")
            .and_then(|result| result.get("message_id"))
            .and_then(Value::as_i64),
    }
}

/// Sonarr maps a Telegram failure to the offending settings field, but only
/// inside `Test` (`TelegramProxy.cs:94-115`) — a live send just logs. Scryer's
/// typed error lane exists on every send, so the same distinction is made every
/// time and the operator is always told which field to fix.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let answer = parse_telegram_body(body);
    let detail = answer.detail(status);

    // A 2xx that does not explicitly say `ok: false` is a delivery. A missing
    // `ok` is treated as success on purpose: local Bot API servers and proxies
    // in front of them are documented deployments
    // (core.telegram.org/bots/api#using-a-local-bot-api-server).
    if (200..300).contains(&status) && answer.ok != Some(false) {
        let mut response = ok_response();
        response.delivery_id = answer.message_id.map(|id| id.to_string());
        response.warnings = warnings;
        return PluginResult::Ok(response);
    }

    match status {
        // Flood control. The wait is in `parameters.retry_after`, not a header,
        // but a proxy may add `Retry-After`; both are whole seconds.
        429 => {
            let mut failure =
                error_response(format!("HTTP 429: {detail}"), Some("http_429".to_string()));
            failure.retry_after_seconds = answer
                .retry_after
                .or_else(|| header(headers, "retry-after").and_then(|value| value.parse().ok()))
                .filter(|seconds| *seconds >= 0)
                .map(|seconds: i64| seconds.max(1));
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        // 401 Unauthorized is always the token. A 404 on `/bot<token>/METHOD` is
        // the same thing — that bot path does not exist — unless Telegram says
        // otherwise in the description.
        401 => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!("bot_token was rejected by Telegram: {detail}"),
            Some(format!("HTTP {status}: {detail}")),
        )),
        404 if !answer.describes("chat not found") => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!(
                "bot_token is not a valid Telegram bot token (HTTP 404): {detail}. Check the token @BotFather issued."
            ),
            Some(format!("HTTP 404: {detail}")),
        )),
        // 403 is always about the chat relationship: blocked by the user,
        // kicked, or not a member of the group/channel. Sonarr blames the bot
        // token here; the chat is the setting the operator has to change.
        403 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("the bot is not allowed to post to the configured chat_id: {detail}"),
            Some(format!("HTTP 403: {detail}")),
        )),
        _ if is_chat_error(&answer) => {
            let mut message = format!("chat_id was rejected by Telegram: {detail}");
            // ResponseParameters.migrate_to_chat_id — the structured field
            // Sonarr only ever read as a description string.
            if let Some(migrated) = answer.migrate_to_chat_id {
                message.push_str(&format!(". The chat's new id is {migrated}"));
            }
            PluginResult::Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                message,
                Some(format!("HTTP {status}: {detail}")),
            ))
        }
        _ if answer.describes("message thread not found")
            || answer.describes("topic_closed")
            || answer.describes("topic not found") =>
        {
            PluginResult::Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("topic_id was rejected by Telegram: {detail}"),
                Some(format!("HTTP {status}: {detail}")),
            ))
        }
        // The markup this plugin built is wrong, or the message is over a limit
        // it should have enforced. Retrying changes nothing and the operator has
        // nothing to fix — this is a plugin bug and is reported as one.
        _ if answer.describes("can't parse entities")
            || answer.describes("message is too long")
            || answer.describes("message text is empty") =>
        {
            PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                format!("Telegram rejected the message this plugin built: {detail}"),
                Some(format!("HTTP {status}: {detail}")),
            ))
        }
        // Sonarr's fall-through for a 400 is `BotToken` (`TelegramProxy.cs:101`).
        400 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "Telegram rejected the request (HTTP 400): {detail}. Check bot_token and chat_id."
            ),
            Some(format!("HTTP 400: {detail}")),
        )),
        // 5xx and anything else is the provider saying no right now: the
        // delivery lane, not the configuration lane.
        _ => {
            let mut failure = error_response(
                format!("HTTP {status}: {detail}"),
                Some(format!("http_{status}")),
            );
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
    }
}

/// `TelegramProxy.cs:103-106`, plus the empty/invalid chat-id cases Telegram
/// reports the same way.
fn is_chat_error(answer: &TelegramBody) -> bool {
    answer.describes("chat not found")
        || answer.describes("group chat was upgraded to a supergroup chat")
        || answer.describes("chat_id is empty")
        || answer.describes("chat_id is invalid")
        || answer.migrate_to_chat_id.is_some()
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
        NotificationMediaUpdateType, NotificationSeverity, PluginNotificationApp,
        PluginNotificationApplicationUpdate, PluginNotificationDownload,
        PluginNotificationExternalIds, PluginNotificationFile, PluginNotificationHealth,
        PluginNotificationImport, PluginNotificationManualInteraction, PluginNotificationMediaFile,
        PluginNotificationMediaUpdate, PluginNotificationRelease, PluginNotificationTitle,
    };

    fn settings() -> Settings {
        Settings {
            bot_token: "bot-token".to_string(),
            chat_id: "-1001".to_string(),
            topic_id: None,
            send_silently: false,
            include_app_name_in_title: false,
            metadata_links: Vec::new(),
            link_preview: LINK_PREVIEW_NONE.to_string(),
        }
    }

    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: None,
            occurred_at: Some("2026-09-01T12:00:00Z".to_string()),
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

    fn text_of(payload: &Value) -> String {
        payload["text"].as_str().expect("text").to_string()
    }

    // -----------------------------------------------------------------
    // Descriptor
    // -----------------------------------------------------------------

    #[test]
    fn descriptor_carries_sonarrs_two_link_settings_and_the_legacy_keys() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("telegram must describe a notification provider");
        };

        let keys: Vec<&str> = notification
            .config_fields
            .iter()
            .map(|field| field.key.as_str())
            .collect();
        // The June keys are a public contract and all survive.
        for key in [
            "bot_token",
            "chat_id",
            "topic_id",
            "send_silently",
            "include_app_name_in_title",
            "include_instance_name_in_title",
        ] {
            assert!(keys.contains(&key), "{key} must remain a config field");
        }
        assert!(keys.contains(&"metadata_links"));
        assert!(keys.contains(&"link_preview"));

        let metadata = notification
            .config_fields
            .iter()
            .find(|field| field.key == "metadata_links")
            .expect("metadata_links");
        assert!(matches!(metadata.field_type, ConfigFieldType::Tag));
        let options: Vec<&str> = metadata
            .options
            .iter()
            .map(|option| option.value.as_str())
            .collect();
        for option in ["imdb", "tvdb", "tvmaze", "trakt"] {
            assert!(options.contains(&option), "Sonarr offers {option}");
        }

        let preview = notification
            .config_fields
            .iter()
            .find(|field| field.key == "link_preview")
            .expect("link_preview");
        assert!(matches!(preview.field_type, ConfigFieldType::Select));
        assert_eq!(preview.default_value.as_deref(), Some("none"));
        // Sonarr comments TVDb out of the preview enum: thetvdb serves no
        // preview data.
        assert!(
            !preview.options.iter().any(|option| option.value == "tvdb"),
            "tvdb cannot be a preview target"
        );

        assert_eq!(
            notification.allowed_hosts,
            vec!["api.telegram.org".to_string()]
        );
        assert!(notification.capabilities.supports_rich_text);
        assert!(!notification.capabilities.supports_images);
        assert!(
            notification
                .capabilities
                .event_options
                .supports_upgrade_filter
        );
    }

    // -----------------------------------------------------------------
    // Settings validation
    // -----------------------------------------------------------------

    #[test]
    fn topic_id_must_be_greater_than_one_or_empty() {
        assert_eq!(parse_topic_id(None).unwrap(), None);
        assert_eq!(parse_topic_id(Some("2")).unwrap(), Some(2));

        let error = parse_topic_id(Some("1")).expect_err("1 is rejected");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("topic_id"));

        let error = parse_topic_id(Some("General")).expect_err("a word is rejected");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("topic_id"));
    }

    #[test]
    fn an_unknown_metadata_link_names_its_field() {
        let error = validated_metadata_links(&["imdb".to_string(), "letterboxd".to_string()])
            .expect_err("letterboxd is not an option");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("metadata_links"));

        assert_eq!(
            validated_metadata_links(&["IMDb".to_string(), "imdb".to_string()]).unwrap(),
            vec!["imdb".to_string()],
            "values are case-insensitive and de-duplicated"
        );
    }

    #[test]
    fn link_preview_must_be_one_of_the_selected_links() {
        let selected = vec!["tvdb".to_string(), "imdb".to_string()];
        assert_eq!(
            validated_link_preview(Some("imdb"), &selected, true).unwrap(),
            "imdb"
        );
        assert_eq!(
            validated_link_preview(None, &selected, true).unwrap(),
            "none",
            "an unset preview is None, as in Sonarr's constructor"
        );
        assert_eq!(
            validated_link_preview(Some("none"), &[], true).unwrap(),
            "none",
            "None is valid with no links selected"
        );

        let error = validated_link_preview(Some("trakt"), &selected, true)
            .expect_err("trakt is not among the selected links (strict, Test-time)");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("link_preview"));

        assert_eq!(
            validated_link_preview(Some("trakt"), &selected, false).unwrap(),
            "trakt",
            "a live send keeps delivering; the mismatch degrades to no preview"
        );

        for strict in [true, false] {
            let error = validated_link_preview(Some("plex"), &selected, strict)
                .expect_err("plex is not an option");
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(error.public_message.contains("link_preview"));
        }
    }

    #[test]
    fn an_unselected_preview_site_disables_the_preview_with_a_warning_on_a_live_send() {
        let mut settings = settings();
        settings.metadata_links = vec!["imdb".to_string()];
        settings.link_preview = "trakt".to_string();
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let (payload, warnings) = build_payload(&req, &settings);
        assert_eq!(
            payload["link_preview_options"],
            json!({ "is_disabled": true })
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("link_preview 'trakt'")),
            "warnings: {warnings:?}"
        );
        assert!(
            text_of(&payload).contains("imdb.com"),
            "the selected links still render"
        );
    }

    // -----------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------

    #[test]
    fn the_sparse_shape_the_core_sends_today_renders_a_heading_and_a_summary() {
        let (payload, warnings) = build_payload(&request(NotificationEventType::Grab), &settings());
        assert_eq!(
            text_of(&payload),
            "<b>Grabbed: Example Show</b>\nGrabbed 'Example.Show.S01E01' for 'Example Show'."
        );
        assert_eq!(payload["parse_mode"], "HTML");
        assert_eq!(payload["chat_id"], "-1001");
        assert_eq!(payload["disable_notification"], false);
        assert_eq!(
            payload["link_preview_options"],
            json!({"is_disabled": true})
        );
        assert!(payload.get("message_thread_id").is_none());
        assert!(warnings.is_empty());
    }

    #[test]
    fn the_app_name_brands_the_heading_and_the_instance_toggle_is_a_no_op() {
        let mut settings = settings();
        settings.include_app_name_in_title = true;
        let (payload, _) = build_payload(&request(NotificationEventType::Grab), &settings);
        assert!(text_of(&payload).starts_with("<b>Scryer - Grabbed: Example Show</b>"));
        // The June port appended `app.name` a second time for
        // `include_instance_name_in_title`; there is no instance name in the
        // contract, so the heading is branded at most once.
        assert_eq!(text_of(&payload).matches("Scryer").count(), 1);
    }

    #[test]
    fn silent_delivery_and_a_forum_topic_reach_the_payload() {
        let mut settings = settings();
        settings.send_silently = true;
        settings.topic_id = Some(42);
        let (payload, _) = build_payload(&request(NotificationEventType::Grab), &settings);
        assert_eq!(payload["disable_notification"], true);
        assert_eq!(payload["message_thread_id"], 42);
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

        let (payload, _) = build_payload(&req, &settings());
        let text = text_of(&payload);
        assert!(text.contains("<b>Episode:</b> 1x01 - Pilot"), "{text}");
        assert!(text.contains("<b>Quality:</b> WEBDL-1080p"), "{text}");
        assert!(
            text.contains("<b>Release:</b> Example.Show.S01E01.1080p.WEB-DL"),
            "{text}"
        );
        assert!(text.contains("<b>Release Group:</b> GROUP"), "{text}");
        assert!(text.contains("<b>Indexer:</b> Example Indexer"), "{text}");
        assert!(text.contains("<b>Size:</b> 2 GB"), "{text}");
        assert!(text.contains("<b>Client:</b> Weaver"), "{text}");
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

        let text = text_of(&build_payload(&req, &settings()).0);
        assert!(
            text.starts_with("<b>Download failed: Example Show</b>"),
            "{text}"
        );
        assert!(text.contains("<b>Status:</b> failed"), "{text}");
        assert!(
            !text.contains("Destination"),
            "a failed download has no import path: {text}"
        );
    }

    #[test]
    fn an_import_renders_the_destination_path_and_the_client() {
        let mut req = request(NotificationEventType::ImportComplete);
        req.summary_title = "Import complete: Example Show".to_string();
        req.summary_message = "Imported 1 file for 'Example Show'.".to_string();
        req.import = Some(PluginNotificationImport {
            dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            source_title: Some("Example.Show.S01E01".to_string()),
            ..PluginNotificationImport::default()
        });
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            ..PluginNotificationDownload::default()
        });

        let text = text_of(&build_payload(&req, &settings()).0);
        assert!(
            text.contains("<b>Destination:</b> /media/TV/Example Show/S01E01.mkv"),
            "{text}"
        );
        assert!(text.contains("<b>Client:</b> Weaver"), "{text}");
        assert!(
            text.contains("<b>Release:</b> Example.Show.S01E01"),
            "{text}"
        );
    }

    #[test]
    fn a_file_delete_names_the_deleted_file() {
        let mut req = request(NotificationEventType::FileDeleted);
        req.summary_title = "Deleted: Example Show".to_string();
        req.file = Some(PluginNotificationFile {
            primary_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Example Show/S01E01.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });
        let text = text_of(&build_payload(&req, &settings()).0);
        assert!(
            text.contains("<b>File:</b> /media/TV/Example Show/S01E01.mkv"),
            "{text}"
        );
    }

    #[test]
    fn health_and_update_events_render_their_own_blocks() {
        let mut health = request(NotificationEventType::HealthIssue);
        health.summary_title = "Health issue".to_string();
        health.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable".to_string()),
            ..PluginNotificationHealth::default()
        });
        let text = text_of(&build_payload(&health, &settings()).0);
        assert!(text.contains("<b>Check:</b> IndexerStatusCheck"), "{text}");
        assert!(
            text.contains("<b>Detail:</b> Indexers unavailable"),
            "{text}"
        );

        let mut update = request(NotificationEventType::ApplicationUpdate);
        update.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            ..PluginNotificationApplicationUpdate::default()
        });
        let text = text_of(&build_payload(&update, &settings()).0);
        assert!(text.contains("<b>Previous Version:</b> 0.19.7"), "{text}");
        assert!(text.contains("<b>New Version:</b> 0.19.8"), "{text}");
    }

    #[test]
    fn manual_interaction_renders_its_reason_and_link() {
        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            kind: Some("import".to_string()),
            reason: Some("Ambiguous episode match".to_string()),
            link: Some("https://scryer.example/queue".to_string()),
        });
        let text = text_of(&build_payload(&req, &settings()).0);
        assert!(
            text.contains("<b>Reason:</b> Ambiguous episode match"),
            "{text}"
        );
        assert!(
            text.contains("<a href=\"https://scryer.example/queue\">Open in Scryer</a>"),
            "{text}"
        );
    }

    #[test]
    fn a_scryer_only_event_renders_generically_rather_than_failing() {
        let mut req = request(NotificationEventType::SubtitleDownloaded);
        req.summary_title = "Subtitles downloaded: Example Show".to_string();
        req.media_files = vec![PluginNotificationMediaFile {
            path: "/media/TV/Example Show/S01E01.mkv".to_string(),
            subtitle_languages: vec!["English".to_string(), "German".to_string()],
            ..PluginNotificationMediaFile::default()
        }];
        let text = text_of(&build_payload(&req, &settings()).0);
        assert!(
            text.starts_with("<b>Subtitles downloaded: Example Show</b>"),
            "{text}"
        );
        assert!(text.contains("<b>Languages:</b> English, German"), "{text}");
    }

    #[test]
    fn a_test_message_carries_a_scryer_link() {
        let mut req = request(NotificationEventType::Test);
        req.summary_title = "Scryer Test Notification".to_string();
        req.summary_message = "This is a test notification from Scryer.".to_string();
        let text = text_of(&build_payload(&req, &settings()).0);
        assert!(
            text.contains("<a href=\"https://github.com/scryer-media/scryer\">Scryer</a>"),
            "{text}"
        );
    }

    // -----------------------------------------------------------------
    // Escaping and limits
    // -----------------------------------------------------------------

    #[test]
    fn telegram_html_entities_are_escaped_in_text_and_in_hrefs() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_title = "Grabbed: <Show> & \"Friends\"".to_string();
        req.summary_message = "a < b & c > d".to_string();
        req.title = Some(series_title());

        let mut settings = settings();
        settings.metadata_links = vec!["tvdb".to_string()];

        let text = text_of(&build_payload(&req, &settings).0);
        assert!(
            text.starts_with("<b>Grabbed: &lt;Show&gt; &amp; &quot;Friends&quot;</b>"),
            "{text}"
        );
        assert!(text.contains("a &lt; b &amp; c &gt; d"), "{text}");
        // Sonarr writes the href raw, leaving a bare `&` in an attribute.
        assert!(
            text.contains("<a href=\"https://thetvdb.com/?tab=series&amp;id=12345\">TVDb</a>"),
            "{text}"
        );
    }

    #[test]
    fn the_message_is_trimmed_to_telegrams_rendered_character_limit() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_message = "x".repeat(6000);
        let (payload, warnings) = build_payload(&req, &settings());
        let text = text_of(&payload);

        // The heading's markup does not count: Telegram measures the text after
        // entity parsing.
        let rendered = text.replace("<b>", "").replace("</b>", "");
        assert_eq!(rendered.chars().count(), MESSAGE_CHARACTER_LIMIT);
        assert!(text.ends_with('…'), "the cut is marked with an ellipsis");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("4096"), "{warnings:?}");
    }

    #[test]
    fn escaping_does_not_eat_the_character_budget() {
        // 3000 ampersands escape to 18000 characters of markup but are still
        // 3000 characters to Telegram, so nothing is trimmed.
        let mut req = request(NotificationEventType::Grab);
        req.summary_title = "T".to_string();
        req.summary_message = "&".repeat(3000);
        let (payload, warnings) = build_payload(&req, &settings());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(text_of(&payload).matches("&amp;").count(), 3000);
    }

    #[test]
    fn a_trimmed_line_never_cuts_a_tag_in_half() {
        let lines = vec![
            Line::Heading("Heading".to_string()),
            Line::Labeled("Release", "y".repeat(6000)),
        ];
        let (text, warnings) = render_message(&lines);
        assert!(
            text.starts_with("<b>Heading</b>\n<b>Release:</b> "),
            "{text}"
        );
        assert!(text.ends_with('…'));
        assert_eq!(warnings.len(), 1);
        assert!(!warnings[0].contains("dropped"), "{warnings:?}");
    }

    #[test]
    fn a_line_that_cannot_be_shortened_usefully_is_dropped_and_counted() {
        let lines = vec![
            Line::Heading("H".repeat(MESSAGE_CHARACTER_LIMIT)),
            Line::Link {
                label: "TVDb".to_string(),
                url: "https://thetvdb.com/?tab=series&id=1".to_string(),
            },
            Line::Plain("tail".to_string()),
        ];
        let (text, warnings) = render_message(&lines);
        assert!(!text.contains("<a href"), "the link did not fit: {text}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("2 line(s) dropped"), "{warnings:?}");
    }

    // -----------------------------------------------------------------
    // Metadata links and preview
    // -----------------------------------------------------------------

    #[test]
    fn only_the_selected_links_with_an_id_are_rendered_in_the_operators_order() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let selected = vec![
            "trakt".to_string(),
            "imdb".to_string(),
            "tmdb".to_string(),
            "tvmaze".to_string(),
        ];
        let links = selected_metadata_links(&req, &selected);
        assert_eq!(
            links
                .iter()
                .map(|(key, label, url)| (key.as_str(), *label, url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "trakt",
                    "Trakt",
                    "https://trakt.tv/search/tvdb/12345?id_type=show"
                ),
                ("imdb", "IMDb", "https://www.imdb.com/title/tt0903747"),
                // tmdb is selected but the series carries no TMDb id.
                ("tvmaze", "TVMaze", "https://www.tvmaze.com/shows/82"),
            ]
        );
    }

    #[test]
    fn the_facet_decides_what_trakt_and_tmdb_mean() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(movie_title());
        let links = selected_metadata_links(&req, &["trakt".to_string(), "tmdb".to_string()]);
        assert_eq!(
            links
                .iter()
                .map(|(_, _, url)| url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://trakt.tv/search/tmdb/603?id_type=movie",
                "https://www.themoviedb.org/movie/603",
            ]
        );
    }

    #[test]
    fn anime_ids_render_from_the_typed_fields_and_from_by_source() {
        let mut req = request(NotificationEventType::Grab);
        let mut title = series_title();
        title.facet = "anime".to_string();
        title.external_ids.anidb_id = Some("979".to_string());
        title
            .external_ids
            .by_source
            .insert("anilist".to_string(), vec!["1535".to_string()]);
        req.title = Some(title);

        let links = selected_metadata_links(
            &req,
            &[
                "anidb".to_string(),
                "anilist".to_string(),
                "mal".to_string(),
            ],
        );
        assert_eq!(
            links
                .iter()
                .map(|(_, _, url)| url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://anidb.net/anime/979",
                "https://anilist.co/anime/1535",
            ]
        );
    }

    #[test]
    fn a_request_with_no_title_block_renders_no_links() {
        let req = request(NotificationEventType::HealthIssue);
        assert!(selected_metadata_links(&req, &["imdb".to_string()]).is_empty());
    }

    #[test]
    fn the_preview_url_is_the_selected_link_and_falls_back_to_disabled() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let mut settings = settings();
        settings.metadata_links = vec!["tvdb".to_string(), "imdb".to_string()];
        settings.link_preview = "imdb".to_string();
        let (payload, _) = build_payload(&req, &settings);
        assert_eq!(
            payload["link_preview_options"],
            json!({"is_disabled": false, "url": "https://www.imdb.com/title/tt0903747"})
        );

        // Sonarr would send `is_disabled: false` with no url here and let
        // Telegram preview the first URL it finds; an unavailable target
        // disables the preview instead.
        let mut without_id = req.clone();
        let mut title = series_title();
        title.external_ids.imdb_id = None;
        without_id.title = Some(title);
        let (payload, _) = build_payload(&without_id, &settings);
        assert_eq!(
            payload["link_preview_options"],
            json!({"is_disabled": true})
        );
    }

    // -----------------------------------------------------------------
    // Error classification
    // -----------------------------------------------------------------

    fn classify(status: u16, body: &str) -> PluginResult<PluginNotificationResponse> {
        classify_response(status, &BTreeMap::new(), body.as_bytes(), Vec::new())
    }

    fn expect_error(result: PluginResult<PluginNotificationResponse>) -> PluginError {
        match result {
            PluginResult::Err(error) => error,
            PluginResult::Ok(response) => panic!("expected a typed error, got {response:?}"),
        }
    }

    fn expect_delivery_failure(
        result: PluginResult<PluginNotificationResponse>,
    ) -> PluginNotificationResponse {
        match result {
            PluginResult::Ok(response) => {
                assert!(
                    !response.success,
                    "expected a failed delivery: {response:?}"
                );
                response
            }
            PluginResult::Err(error) => panic!("expected a delivery failure, got {error:?}"),
        }
    }

    #[test]
    fn a_success_reports_the_message_id_as_the_delivery_id() {
        let PluginResult::Ok(response) =
            classify(200, r#"{"ok":true,"result":{"message_id":4242}}"#)
        else {
            panic!("a 200 is a delivery");
        };
        assert!(response.success);
        assert_eq!(response.delivery_id.as_deref(), Some("4242"));
    }

    #[test]
    fn a_two_hundred_without_an_ok_field_is_still_a_delivery() {
        // Local Bot API servers and proxies are documented deployments.
        let PluginResult::Ok(response) = classify(200, "{}") else {
            panic!("a 200 is a delivery");
        };
        assert!(response.success);
    }

    #[test]
    fn a_rejected_bot_token_is_an_auth_failure_naming_the_field() {
        for (status, body) in [
            (
                401,
                r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
            ),
            (
                404,
                r#"{"ok":false,"error_code":404,"description":"Not Found"}"#,
            ),
        ] {
            let error = expect_error(classify(status, body));
            assert_eq!(error.code, PluginErrorCode::AuthFailed, "status {status}");
            assert!(
                error.public_message.contains("bot_token"),
                "status {status}: {}",
                error.public_message
            );
        }
    }

    #[test]
    fn sonarrs_chat_id_descriptions_name_the_chat_id_field() {
        for description in [
            "Bad Request: chat not found",
            "Bad Request: group chat was upgraded to a supergroup chat",
        ] {
            let error = expect_error(classify(
                400,
                &format!(r#"{{"ok":false,"error_code":400,"description":"{description}"}}"#),
            ));
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(
                error.public_message.contains("chat_id"),
                "{}",
                error.public_message
            );
        }
    }

    #[test]
    fn a_migrated_group_reports_the_new_chat_id_from_response_parameters() {
        let error = expect_error(classify(
            400,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: group chat was upgraded to a supergroup chat","parameters":{"migrate_to_chat_id":-1001234567890}}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(
            error.public_message.contains("-1001234567890"),
            "{}",
            error.public_message
        );
    }

    #[test]
    fn a_missing_forum_topic_names_the_topic_id_field() {
        let error = expect_error(classify(
            400,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: message thread not found"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(
            error.public_message.contains("topic_id"),
            "{}",
            error.public_message
        );
    }

    #[test]
    fn a_markup_rejection_is_this_plugins_bug_not_the_operators() {
        let error = expect_error(classify(
            400,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse entities: Unsupported start tag"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::Permanent);
    }

    #[test]
    fn an_unrecognised_four_hundred_falls_through_to_sonarrs_bot_token_default() {
        let error = expect_error(classify(
            400,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: something new"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("bot_token"));
    }

    #[test]
    fn a_forbidden_chat_is_a_configuration_error() {
        let error = expect_error(classify(
            403,
            r#"{"ok":false,"error_code":403,"description":"Forbidden: bot was blocked by the user"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("chat_id"));
    }

    #[test]
    fn flood_control_reports_the_retry_after_telegram_asked_for() {
        let response = expect_delivery_failure(classify(
            429,
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 37","parameters":{"retry_after":37}}"#,
        ));
        assert_eq!(response.retry_after_seconds, Some(37));
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));
    }

    #[test]
    fn a_retry_after_header_is_used_when_the_body_carries_none() {
        let headers = BTreeMap::from([("Retry-After".to_string(), "12".to_string())]);
        let response = expect_delivery_failure(classify_response(
            429,
            &headers,
            br#"{"ok":false,"error_code":429,"description":"Too Many Requests"}"#,
            Vec::new(),
        ));
        assert_eq!(response.retry_after_seconds, Some(12));
    }

    #[test]
    fn a_server_error_stays_in_the_delivery_lane() {
        let response = expect_delivery_failure(classify(502, "upstream exploded"));
        assert_eq!(response.provider_status.as_deref(), Some("http_502"));
        assert!(
            response
                .error
                .as_deref()
                .unwrap()
                .contains("upstream exploded"),
            "{response:?}"
        );
    }

    #[test]
    fn a_truncation_warning_survives_a_failed_delivery() {
        let response = expect_delivery_failure(classify_response(
            502,
            &BTreeMap::new(),
            b"{}",
            vec!["message trimmed".to_string()],
        ));
        assert_eq!(response.warnings, vec!["message trimmed".to_string()]);
    }

    #[test]
    fn format_bytes_matches_sonarrs_rounding() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(1_572_864), "1.5 MB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
    }
}
