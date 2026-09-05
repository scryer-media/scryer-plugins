//! Discord webhook notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Discord notification (`src/NzbDrone.Core/Notifications/Discord/Discord.cs`)
//! is not one renderer: it is eleven, one per event, each driven by an operator
//! configured field set (`DiscordGrabFieldType` / `DiscordImportFieldType` /
//! `DiscordManualInteractionFieldType`). The June port collapsed all of them into
//! a single four-field embed, which meant a grab and a health issue looked the
//! same and nothing an operator had configured in Sonarr could be expressed here.
//!
//! This module rebuilds that per-event rendering on Scryer's notification
//! contract:
//!
//! * three `Tag` config fields carry Sonarr's option lists and defaults, so the
//!   settings are recognisable to anyone migrating from Sonarr;
//! * each option renders only when the contract actually carries the data. Two
//!   of Sonarr's options — `Rating` and `Genres` — have **no carrier at all** in
//!   `PluginNotificationTitle`, so they are accepted, kept in the descriptor for
//!   config parity, and render nothing (see the README and the report's
//!   out-of-fence findings);
//! * the heading follows `Discord.GetTitle` (series - SxxExx - episode titles,
//!   backticks escaped, 256-character cap) and the link line follows
//!   `Discord.GetLinksString`, generalised from Sonarr's TVDB-only world to
//!   Scryer's facets (series/anime vs movie) and external-id set;
//! * the colour table is `DiscordColors` verbatim, with Scryer's `severity` as
//!   an override Sonarr has no equivalent for.
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper turns every non-2xx into one shape — `error_response("HTTP
//! N: body", "http_N")` — which loses every distinction Discord's API makes.
//! Discord answers `204` on success (`200` with the message body when
//! `wait=true`), `400` for a payload this plugin built wrong, `401`/`403`/`404`
//! for a webhook URL that is no longer usable, and `429` with a `retry_after`
//! the core can act on. Those are three different lanes in Scryer's contract —
//! typed `PluginError`, delivery failure with `retry_after_seconds`, and plain
//! delivery failure — so the send lives here.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationSeverity,
    PluginNotificationEpisode, current_sdk_constraint,
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

const PROVIDER_TYPE: &str = "discord";
const USER_AGENT: &str = concat!("scryer-discord-plugin/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// Discord's documented limits
//
// https://docs.discord.com/developers/resources/message (Embed Limits) and
// https://docs.discord.com/developers/resources/webhook (Execute Webhook).
// ---------------------------------------------------------------------------

const EMBED_TITLE_LIMIT: usize = 256;
const EMBED_DESCRIPTION_LIMIT: usize = 4096;
const EMBED_FIELD_COUNT_LIMIT: usize = 25;
const EMBED_FIELD_NAME_LIMIT: usize = 256;
const EMBED_FIELD_VALUE_LIMIT: usize = 1024;
const EMBED_FOOTER_LIMIT: usize = 2048;
const EMBED_AUTHOR_NAME_LIMIT: usize = 256;
const MESSAGE_CHARACTER_LIMIT: usize = 6000;
const CONTENT_LIMIT: usize = 2000;

/// Sonarr's own cut for the Overview field (`Discord.cs:79`), not a Discord
/// limit: it keeps a long synopsis from swallowing the embed.
const OVERVIEW_LIMIT: usize = 300;

// ---------------------------------------------------------------------------
// DiscordColors.cs
// ---------------------------------------------------------------------------

const COLOR_DANGER: i64 = 15_749_200;
const COLOR_SUCCESS: i64 = 2_605_644;
const COLOR_WARNING: i64 = 16_753_920;
const COLOR_STANDARD: i64 = 16_761_392;
const COLOR_UPGRADE: i64 = 4_089_856;

// ---------------------------------------------------------------------------
// Field sets (DiscordFieldType.cs; defaults from DiscordSettings.cs:19-66)
// ---------------------------------------------------------------------------

const GRAB_FIELD_OPTIONS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("rating", "Rating"),
    ("genres", "Genres"),
    ("quality", "Quality"),
    ("group", "Group"),
    ("size", "Size"),
    ("links", "Links"),
    ("release", "Release"),
    ("poster", "Poster"),
    ("fanart", "Fanart"),
    ("indexer", "Indexer"),
    ("custom_formats", "Custom Formats"),
    ("custom_format_score", "Custom Format Score"),
];

const GRAB_FIELD_DEFAULTS: &[&str] = &[
    "overview",
    "rating",
    "genres",
    "quality",
    "group",
    "size",
    "links",
    "release",
    "poster",
    "fanart",
    "indexer",
    "custom_formats",
    "custom_format_score",
];

const IMPORT_FIELD_OPTIONS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("rating", "Rating"),
    ("genres", "Genres"),
    ("quality", "Quality"),
    ("codecs", "Codecs"),
    ("group", "Group"),
    ("size", "Size"),
    ("languages", "Languages"),
    ("subtitles", "Subtitles"),
    ("links", "Links"),
    ("release", "Release"),
    ("poster", "Poster"),
    ("fanart", "Fanart"),
    ("custom_formats", "Custom Formats"),
    ("custom_format_score", "Custom Format Score"),
];

/// Sonarr ships Import without the two custom-format options
/// (`DiscordSettings.cs:37-52`); the operator opts in.
const IMPORT_FIELD_DEFAULTS: &[&str] = &[
    "overview",
    "rating",
    "genres",
    "quality",
    "codecs",
    "group",
    "size",
    "languages",
    "subtitles",
    "links",
    "release",
    "poster",
    "fanart",
];

const MANUAL_FIELD_OPTIONS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("rating", "Rating"),
    ("genres", "Genres"),
    ("quality", "Quality"),
    ("group", "Group"),
    ("size", "Size"),
    ("links", "Links"),
    ("download_title", "Download Title"),
    ("poster", "Poster"),
    ("fanart", "Fanart"),
];

const MANUAL_FIELD_DEFAULTS: &[&str] = &[
    "overview",
    "rating",
    "genres",
    "quality",
    "group",
    "size",
    "links",
    "download_title",
    "poster",
    "fanart",
];

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

/// Built here rather than through `notify_common::build_notification_descriptor`
/// because that helper cannot express `event_options`: Discord implements
/// `OnGrab`, `OnDownload` (hence `SupportsOnUpgrade`), `OnEpisodeFileDelete` and
/// `OnHealthIssue` in Sonarr (`NotificationBase.cs:96-108`), so all three
/// operator-facing event filters are meaningful for this channel.
fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Discord".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            default_base_url: None,
            // Deliberately unrestricted. Discord's own hosts are `discord.com`
            // and the legacy `discordapp.com`, but Discord-shaped webhook
            // endpoints are also served by proxies and self-hosted relays that
            // operators point this channel at; an allowlist here would break
            // them with no security gain the host's egress policy does not
            // already provide.
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                supports_rich_text: true,
                supports_images: true,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![
                    NotificationDeliveryMode::Chat,
                    NotificationDeliveryMode::Webhook,
                ],
                payload_formats: vec![
                    NotificationPayloadFormat::PlainText,
                    NotificationPayloadFormat::RichEmbed,
                ],
                supported_events: general_notification_events(),
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
            "webhook_url",
            "Webhook URL",
            true,
            None,
            Some("The Discord channel's incoming-webhook URL."),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some("Overrides the display name Discord shows for the webhook."),
        ),
        connection_field(
            "avatar",
            "Avatar URL",
            false,
            None,
            Some("Overrides the avatar image Discord shows for the webhook."),
        ),
        field(
            "author",
            "Author",
            ConfigFieldType::String,
            false,
            None,
            Some("Embed author label; defaults to the Scryer application name."),
        ),
        connection_field(
            "author_icon_url",
            "Author Icon URL",
            false,
            None,
            Some(
                "Optional icon shown beside the embed author. Discord renders PNG, JPEG, GIF and WebP; SVG is not rendered.",
            ),
        ),
        tag_field(
            "grab_fields",
            "On Grab Fields",
            GRAB_FIELD_OPTIONS,
            GRAB_FIELD_DEFAULTS,
            "Fields rendered on grab notifications. Rating and Genres carry no data in Scryer yet and render nothing.",
        ),
        tag_field(
            "import_fields",
            "On Import Fields",
            IMPORT_FIELD_OPTIONS,
            IMPORT_FIELD_DEFAULTS,
            "Fields rendered on import, upgrade, import-complete and library-change notifications.",
        ),
        tag_field(
            "manual_interaction_fields",
            "On Manual Interaction Fields",
            MANUAL_FIELD_OPTIONS,
            MANUAL_FIELD_DEFAULTS,
            "Fields rendered when a download needs manual interaction.",
        ),
    ]
}

fn tag_field(
    key: &str,
    label: &str,
    options: &[(&str, &str)],
    defaults: &[&str],
    help_text: &str,
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
        ..field(
            key,
            label,
            ConfigFieldType::Tag,
            false,
            Some(&defaults.join(",")),
            Some(help_text),
        )
    }
}

// ---------------------------------------------------------------------------
// Render options
// ---------------------------------------------------------------------------

/// Everything the renderer needs from configuration, resolved once per send.
///
/// Split out so every builder below is a pure function of `(request, options)`
/// and therefore unit-testable without a host.
#[derive(Debug, Clone)]
struct RenderOptions {
    author: Option<String>,
    author_icon_url: Option<String>,
    grab_fields: Vec<String>,
    import_fields: Vec<String>,
    manual_interaction_fields: Vec<String>,
}

impl RenderOptions {
    fn from_config() -> Self {
        Self {
            author: config_value("author"),
            author_icon_url: config_value("author_icon_url"),
            grab_fields: selected_fields("grab_fields", GRAB_FIELD_DEFAULTS),
            import_fields: selected_fields("import_fields", IMPORT_FIELD_DEFAULTS),
            manual_interaction_fields: selected_fields(
                "manual_interaction_fields",
                MANUAL_FIELD_DEFAULTS,
            ),
        }
    }

    #[cfg(test)]
    fn defaults() -> Self {
        Self {
            author: None,
            author_icon_url: None,
            grab_fields: to_owned(GRAB_FIELD_DEFAULTS),
            import_fields: to_owned(IMPORT_FIELD_DEFAULTS),
            manual_interaction_fields: to_owned(MANUAL_FIELD_DEFAULTS),
        }
    }
}

fn to_owned(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// A `Tag` field that is *absent* falls back to Sonarr's default set; a field
/// the operator has deliberately emptied renders no fields at all. Those are
/// different intentions, and `config_value` cannot tell them apart because it
/// filters empty strings, so read the raw value.
fn selected_fields(key: &str, defaults: &[&str]) -> Vec<String> {
    match config::get(key).ok().flatten() {
        Some(raw) => parse_field_set(&raw),
        None => to_owned(defaults),
    }
}

fn parse_field_set(raw: &str) -> Vec<String> {
    raw.split([',', '\n', ';'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// Build the Discord webhook payload plus any degradation warnings.
///
/// Mirrors `Discord.CreatePayload` (`Discord.cs:652-674`): `content`,
/// `username`, `avatar_url` and `embeds`, with `username`/`avatar_url` emitted
/// only when configured.
fn build_payload(
    req: &PluginNotificationRequest,
    options: &RenderOptions,
    username: Option<&str>,
    avatar: Option<&str>,
) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();
    let mut payload = serde_json::Map::new();

    if req.is_test || req.event_type == NotificationEventType::Test {
        // Sonarr's test is a plain content message with no embed
        // (`Discord.cs:635-650`), which is also the cheapest possible proof
        // that the webhook URL is live.
        payload.insert("content".to_string(), json!(test_message(req)));
    } else {
        if req.event_type == NotificationEventType::Rename {
            // `Discord.OnRename` (`Discord.cs:341-354`) posts the word
            // "Renamed" as message content alongside a title-only embed.
            payload.insert("content".to_string(), json!("Renamed"));
        }
        let mut embed = build_embed(req, options);
        enforce_embed_limits(&mut embed, &mut warnings);
        payload.insert("embeds".to_string(), json!([embed]));
    }

    if let Some(content) = payload.get_mut("content")
        && let Some(text) = content.as_str()
    {
        let (clamped, truncated) = clamp(text, CONTENT_LIMIT);
        if truncated {
            warnings.push(format!(
                "message content truncated to Discord's {CONTENT_LIMIT}-character limit"
            ));
            *content = json!(clamped);
        }
    }

    if let Some(username) = username.filter(|value| !value.trim().is_empty()) {
        payload.insert("username".to_string(), json!(username));
    }
    if let Some(avatar) = avatar.filter(|value| !value.trim().is_empty()) {
        payload.insert("avatar_url".to_string(), json!(avatar));
    }

    (Value::Object(payload), warnings)
}

fn test_message(req: &PluginNotificationRequest) -> String {
    // Sonarr stamps `DateTime.Now`. A component has no guaranteed wall clock,
    // so the event's own timestamp is used when the core sends one and the
    // sentence simply ends early when it does not.
    match req.occurred_at.as_deref().map(str::trim) {
        Some(occurred_at) if !occurred_at.is_empty() => {
            format!("Test message from {} posted at {occurred_at}", req.app.name)
        }
        _ => format!("Test message from {}", req.app.name),
    }
}

fn build_embed(req: &PluginNotificationRequest, options: &RenderOptions) -> Value {
    let mut embed = serde_json::Map::new();

    let author_name = options
        .author
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| req.app.name.clone());
    let mut author = serde_json::Map::new();
    author.insert("name".to_string(), json!(author_name));
    if let Some(icon) = options
        .author_icon_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        author.insert("icon_url".to_string(), json!(icon));
    }
    embed.insert("author".to_string(), Value::Object(author));

    embed.insert("title".to_string(), json!(embed_title(req)));
    if let Some(description) = embed_description(req) {
        embed.insert("description".to_string(), json!(description));
    }
    embed.insert("color".to_string(), json!(embed_color(req)));

    if let Some((_, url)) = metadata_links(req).first() {
        embed.insert("url".to_string(), json!(url));
    }
    if let Some(occurred_at) = req
        .occurred_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        embed.insert("timestamp".to_string(), json!(occurred_at));
    }

    let selected = field_set_for(req, options);
    if let Some(selected) = selected.as_deref() {
        if selected.iter().any(|field| field == "poster")
            && let Some(poster) = title_image(req, |title| title.poster_url.as_deref())
        {
            embed.insert("thumbnail".to_string(), json!({ "url": poster }));
        }
        if selected.iter().any(|field| field == "fanart")
            && let Some(fanart) = title_image(req, |title| title.background_url.as_deref())
        {
            embed.insert("image".to_string(), json!({ "url": fanart }));
        }
    }

    let fields = event_fields(req, selected.as_deref().unwrap_or(&[]));
    if !fields.is_empty() {
        embed.insert("fields".to_string(), Value::Array(fields));
    }

    Value::Object(embed)
}

fn title_image<'a>(
    req: &'a PluginNotificationRequest,
    pick: impl Fn(&'a scryer_plugin_sdk::PluginNotificationTitle) -> Option<&'a str>,
) -> Option<&'a str> {
    req.title
        .as_ref()
        .and_then(pick)
        .map(str::trim)
        .filter(|url| !url.is_empty())
}

/// Which configured field set, if any, governs this event.
///
/// Sonarr drives grab from `GrabFields`, import/upgrade/import-complete and the
/// series add/delete embeds from `ImportFields`, and manual interaction from
/// `ManualInteractionFields`. Events Sonarr renders with a fixed shape (rename,
/// file delete, health, application update) have no set.
fn field_set_for(req: &PluginNotificationRequest, options: &RenderOptions) -> Option<Vec<String>> {
    match req.event_type {
        NotificationEventType::Grab => Some(options.grab_fields.clone()),
        NotificationEventType::Download
        | NotificationEventType::Upgrade
        | NotificationEventType::ImportComplete
        | NotificationEventType::TitleAdded
        | NotificationEventType::TitleDeleted => Some(options.import_fields.clone()),
        NotificationEventType::ManualInteractionRequired => {
            Some(options.manual_interaction_fields.clone())
        }
        _ => None,
    }
}

fn event_fields(req: &PluginNotificationRequest, selected: &[String]) -> Vec<Value> {
    match req.event_type {
        NotificationEventType::Grab
        | NotificationEventType::Download
        | NotificationEventType::Upgrade
        | NotificationEventType::ImportComplete
        | NotificationEventType::ManualInteractionRequired => render_selected_fields(req, selected),
        // `Discord.OnSeriesAdd` / `OnSeriesDelete` (`Discord.cs:387-462`) render
        // exactly one field, Links, with poster/fanart still honoured.
        NotificationEventType::TitleAdded | NotificationEventType::TitleDeleted => {
            render_selected_fields(req, &["links".to_string()])
        }
        // `Discord.OnEpisodeFileDelete` (`Discord.cs:356-385`).
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            let mut fields = Vec::new();
            push_field(&mut fields, "Reason", delete_reason(req), false);
            push_field(
                &mut fields,
                "File name",
                deleted_path(req).map(|path| code_block(&path)),
                false,
            );
            fields
        }
        // `Discord.OnApplicationUpdate` (`Discord.cs:504-534`).
        NotificationEventType::ApplicationUpdate => {
            let update = req.application_update.as_ref();
            let mut fields = Vec::new();
            push_field(
                &mut fields,
                "Previous Version",
                update.and_then(|update| update.current_version.clone()),
                false,
            );
            push_field(
                &mut fields,
                "New Version",
                update.and_then(|update| update.target_version.clone()),
                false,
            );
            fields
        }
        // Health embeds are title + description only in Sonarr.
        NotificationEventType::HealthIssue | NotificationEventType::HealthRestored => Vec::new(),
        NotificationEventType::Rename => Vec::new(),
        NotificationEventType::Test => Vec::new(),
        // Scryer-only events Sonarr has no renderer for. Never fail on an event
        // this channel does not special-case: render what the contract carries.
        _ => generic_fields(req),
    }
}

/// Sonarr iterates the *settings* array, so the operator's order is the render
/// order, and a field whose name or value is empty is dropped
/// (`Discord.cs:125-128`).
fn render_selected_fields(req: &PluginNotificationRequest, selected: &[String]) -> Vec<Value> {
    let mut fields = Vec::new();
    for option in selected {
        match option.as_str() {
            "overview" => push_field(&mut fields, "Overview", overview(req), false),
            // `Rating` and `Genres` are accepted for config parity with Sonarr
            // and render nothing: `PluginNotificationTitle` carries neither a
            // rating nor a genre list. See the report's out-of-fence findings.
            "rating" | "genres" => {}
            "quality" => push_field(&mut fields, "Quality", quality(req), true),
            "codecs" => push_field(&mut fields, "Codecs", codecs(req), true),
            "group" => push_field(&mut fields, "Group", release_group(req), false),
            "size" => push_field(
                &mut fields,
                "Size",
                total_size_bytes(req).map(format_bytes),
                true,
            ),
            "languages" => push_field(
                &mut fields,
                "Languages",
                join_media_languages(req, |file| &file.audio_languages),
                false,
            ),
            "subtitles" => push_field(
                &mut fields,
                "Subtitles",
                join_media_languages(req, |file| &file.subtitle_languages),
                false,
            ),
            "links" => push_field(&mut fields, "Links", links_string(req), false),
            "release" => push_field(
                &mut fields,
                "Release",
                release_title(req).map(|title| code_block(&title)),
                false,
            ),
            "download_title" => push_field(
                &mut fields,
                "Download",
                download_title(req).map(|title| code_block(&title)),
                false,
            ),
            "indexer" => push_field(&mut fields, "Indexer", indexer(req), false),
            "custom_formats" => {
                push_field(&mut fields, "Custom Formats", custom_formats(req), false)
            }
            "custom_format_score" => push_field(
                &mut fields,
                "Custom Format Score",
                custom_format_score(req).map(|score| score.to_string()),
                false,
            ),
            // An unrecognised option is a forward-compatible config, not a
            // failure: skip it and render everything else.
            _ => {}
        }
    }
    fields
}

fn generic_fields(req: &PluginNotificationRequest) -> Vec<Value> {
    let mut fields = Vec::new();
    push_field(&mut fields, "Quality", quality(req), true);
    push_field(&mut fields, "Indexer", indexer(req), true);
    push_field(
        &mut fields,
        "Download Client",
        req.download
            .as_ref()
            .and_then(|download| download.client_name.clone()),
        true,
    );
    push_field(&mut fields, "Links", links_string(req), false);
    fields
}

fn push_field(fields: &mut Vec<Value>, name: &str, value: Option<String>, inline: bool) {
    let Some(value) = value else { return };
    if value.trim().is_empty() {
        return;
    }
    fields.push(json!({ "name": name, "value": value, "inline": inline }));
}

fn code_block(value: &str) -> String {
    format!("```{value}```")
}

// ---------------------------------------------------------------------------
// Heading, description, colour
// ---------------------------------------------------------------------------

fn embed_title(req: &PluginNotificationRequest) -> String {
    match req.event_type {
        NotificationEventType::HealthIssue => health_source(req).unwrap_or_else(|| summary(req)),
        NotificationEventType::HealthRestored => match health_source(req) {
            Some(source) => format!("Health Issue Resolved: {source}"),
            None => summary(req),
        },
        NotificationEventType::ApplicationUpdate => summary(req),
        _ => heading(req),
    }
}

fn summary(req: &PluginNotificationRequest) -> String {
    let summary = req.summary_title.trim();
    if summary.is_empty() {
        req.app.name.clone()
    } else {
        summary.to_string()
    }
}

/// `Discord.GetTitle` (`Discord.cs:711-737`) on Scryer's contract.
///
/// Sonarr composes "Series - {season}x{ep}[x{ep}…] - Episode titles", or
/// "Series - {air date} - Episode title" for a daily series, escapes backticks,
/// and caps at 256 characters. Scryer's contract already carries a rendered
/// `episode.display` for most events; when it does not, the episode list is
/// composed the same way Sonarr composes it.
fn heading(req: &PluginNotificationRequest) -> String {
    let name = req
        .title
        .as_ref()
        .map(|title| title.name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| summary(req));

    let raw = match episode_detail(req) {
        Some(detail) => format!("{name} - {detail}"),
        None => name,
    };

    clamp_heading(&raw.replace('`', "\\`"))
}

fn clamp_heading(title: &str) -> String {
    if title.chars().count() <= EMBED_TITLE_LIMIT {
        return title.to_string();
    }
    let head: String = title.chars().take(EMBED_TITLE_LIMIT - 3).collect();
    format!("{}...", head.trim_end_matches('\\'))
}

fn episode_detail(req: &PluginNotificationRequest) -> Option<String> {
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

    let titles: Vec<&str> = episodes
        .iter()
        .filter_map(|episode| episode.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .collect();
    let titles = titles.join(" + ");

    // Sonarr's daily-series branch keys off `SeriesTypes.Daily`; the contract
    // has no series type, so the observable stand-in is an episode that has an
    // air date and no episode number.
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

/// The short event label Sonarr puts in `Description` ("Episode Grabbed", …).
///
/// Sonarr's wording is series-specific because Sonarr only has series. Scryer
/// carries a facet, so the episode wording is kept where the facet is episodic
/// and neutral wording is used otherwise.
fn event_label(req: &PluginNotificationRequest) -> String {
    let episodic = is_episodic(req);
    match req.event_type {
        NotificationEventType::Grab => episodic_label(episodic, "Episode Grabbed", "Grabbed"),
        NotificationEventType::Download => {
            if is_failure(req) {
                "Download Failed".to_string()
            } else if is_upgrade(req) {
                episodic_label(episodic, "Episode Upgraded", "Upgraded")
            } else {
                episodic_label(episodic, "Episode Imported", "Imported")
            }
        }
        NotificationEventType::Upgrade => episodic_label(episodic, "Episode Upgraded", "Upgraded"),
        NotificationEventType::ImportComplete => "Import Complete".to_string(),
        NotificationEventType::ImportRejected => "Import Rejected".to_string(),
        NotificationEventType::Rename => "Renamed".to_string(),
        NotificationEventType::FileDeleted => {
            episodic_label(episodic, "Episode Deleted", "File Deleted")
        }
        NotificationEventType::FileDeletedForUpgrade => episodic_label(
            episodic,
            "Episode Deleted for Upgrade",
            "File Deleted for Upgrade",
        ),
        NotificationEventType::TitleAdded => episodic_label(episodic, "Series Added", "Added"),
        NotificationEventType::TitleDeleted => {
            episodic_label(episodic, "Series Deleted", "Deleted")
        }
        NotificationEventType::ManualInteractionRequired => "Manual interaction needed".to_string(),
        NotificationEventType::PostProcessingCompleted => "Post-processing Complete".to_string(),
        NotificationEventType::SubtitleDownloaded => "Subtitle Downloaded".to_string(),
        NotificationEventType::SubtitleSearchFailed => "Subtitle Search Failed".to_string(),
        NotificationEventType::MediaRequestSubmitted => "Media Request Submitted".to_string(),
        NotificationEventType::MediaRequestApproved => "Media Request Approved".to_string(),
        NotificationEventType::MediaRequestRejected => "Media Request Rejected".to_string(),
        NotificationEventType::MediaRequestCanceled => "Media Request Canceled".to_string(),
        NotificationEventType::HealthIssue => "Health Issue".to_string(),
        NotificationEventType::HealthRestored => "Health Issue Resolved".to_string(),
        NotificationEventType::ApplicationUpdate => "Application Updated".to_string(),
        NotificationEventType::Test => "Test".to_string(),
    }
}

fn episodic_label(episodic: bool, episodic_label: &str, neutral_label: &str) -> String {
    if episodic {
        episodic_label.to_string()
    } else {
        neutral_label.to_string()
    }
}

/// Sonarr's `Description` is the label alone. Scryer's `summary_message` is the
/// only carrier for several facts the structured blocks do not hold yet (the
/// delete reason, the "imported N files" count, a failure reason), so it is
/// appended on its own line rather than dropped.
fn embed_description(req: &PluginNotificationRequest) -> Option<String> {
    let message = req.summary_message.trim();
    match req.event_type {
        NotificationEventType::HealthIssue => Some(
            req.health
                .as_ref()
                .and_then(|health| health.message.clone())
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| message.to_string()),
        )
        .filter(|description| !description.is_empty()),
        NotificationEventType::HealthRestored => {
            let detail = req
                .health
                .as_ref()
                .and_then(|health| health.message.clone())
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| message.to_string());
            (!detail.is_empty()).then(|| format!("The following issue is now resolved: {detail}"))
        }
        // `Discord.OnSeriesDelete` uses Sonarr's "files deleted / not deleted"
        // sentence as the whole description (`Discord.cs:438`); Scryer carries
        // the equivalent sentence in the summary.
        NotificationEventType::TitleDeleted => Some(if message.is_empty() {
            event_label(req)
        } else {
            message.to_string()
        }),
        NotificationEventType::ApplicationUpdate => req
            .application_update
            .as_ref()
            .and_then(|update| update.summary.clone())
            .filter(|summary| !summary.trim().is_empty())
            .or_else(|| (!message.is_empty()).then(|| message.to_string())),
        NotificationEventType::Rename => None,
        _ => {
            let label = event_label(req);
            if message.is_empty() || message == label {
                Some(label)
            } else {
                Some(format!("{label}\n{message}"))
            }
        }
    }
}

/// `DiscordColors` per event, with Scryer's `severity` as an override Sonarr
/// has no equivalent for. A warning never downgrades an already-red event.
fn embed_color(req: &PluginNotificationRequest) -> i64 {
    let base = match req.event_type {
        NotificationEventType::Grab
        | NotificationEventType::ManualInteractionRequired
        | NotificationEventType::ApplicationUpdate
        | NotificationEventType::Rename
        | NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestCanceled
        | NotificationEventType::Test => COLOR_STANDARD,
        NotificationEventType::Download => {
            if is_failure(req) {
                COLOR_DANGER
            } else if is_upgrade(req) {
                COLOR_UPGRADE
            } else {
                COLOR_SUCCESS
            }
        }
        NotificationEventType::Upgrade => COLOR_UPGRADE,
        NotificationEventType::ImportComplete
        | NotificationEventType::TitleAdded
        | NotificationEventType::HealthRestored
        | NotificationEventType::PostProcessingCompleted
        | NotificationEventType::SubtitleDownloaded
        | NotificationEventType::MediaRequestApproved => COLOR_SUCCESS,
        NotificationEventType::FileDeleted
        | NotificationEventType::FileDeletedForUpgrade
        | NotificationEventType::TitleDeleted
        | NotificationEventType::ImportRejected
        | NotificationEventType::SubtitleSearchFailed
        | NotificationEventType::MediaRequestRejected => COLOR_DANGER,
        NotificationEventType::HealthIssue => COLOR_WARNING,
    };

    match req.severity {
        Some(NotificationSeverity::Error) => COLOR_DANGER,
        Some(NotificationSeverity::Warning) if base != COLOR_DANGER => COLOR_WARNING,
        _ => base,
    }
}

fn is_episodic(req: &PluginNotificationRequest) -> bool {
    req.title
        .as_ref()
        .map(|title| title.facet.to_ascii_lowercase())
        .is_some_and(|facet| matches!(facet.as_str(), "series" | "anime" | "tv" | "show"))
}

/// `NotificationEventType::Download` is not Sonarr's `OnDownload`.
///
/// Scryer's dispatcher maps a **failed** download onto it
/// (`crates/scryer-application/src/notifications/dispatcher.rs:34,418-448`,
/// release-0.19.8), so the renderer has to read the summary and the download
/// status rather than assume a successful import.
fn is_failure(req: &PluginNotificationRequest) -> bool {
    if matches!(req.severity, Some(NotificationSeverity::Error)) {
        return true;
    }
    if let Some(download) = req.download.as_ref()
        && let Some(status) = download.status.as_deref()
        && matches!(
            status.to_ascii_lowercase().as_str(),
            "failed" | "failure" | "error"
        )
    {
        return true;
    }
    req.summary_title.to_ascii_lowercase().contains("failed")
}

fn is_upgrade(req: &PluginNotificationRequest) -> bool {
    req.event_type == NotificationEventType::Upgrade
        || req.import.as_ref().is_some_and(|import| import.upgrade)
}

fn health_source(req: &PluginNotificationRequest) -> Option<String> {
    req.health.as_ref().and_then(|health| {
        health
            .code
            .clone()
            .or_else(|| health.status.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

// ---------------------------------------------------------------------------
// Field values
// ---------------------------------------------------------------------------

fn overview(req: &PluginNotificationRequest) -> Option<String> {
    let overview = req
        .episode
        .as_ref()
        .and_then(|episode| episode.overview.clone())
        .or_else(|| {
            req.episodes
                .first()
                .and_then(|episode| episode.overview.clone())
        })
        .or_else(|| req.title.as_ref().and_then(|title| title.overview.clone()))?;
    let overview = overview.trim();
    if overview.is_empty() {
        return None;
    }
    // Sonarr's exact cut: 300 characters plus a literal ellipsis.
    Some(if overview.chars().count() <= OVERVIEW_LIMIT {
        overview.to_string()
    } else {
        let head: String = overview.chars().take(OVERVIEW_LIMIT).collect();
        format!("{head}...")
    })
}

fn quality(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
        .and_then(|release| release.quality.clone())
        .or_else(|| req.media_files.iter().find_map(|file| file.quality.clone()))
        .map(|quality| quality.trim().to_string())
        .filter(|quality| !quality.is_empty())
}

/// `MediaInfoFormatter` output in Sonarr (`Discord.cs:197-204`), assembled from
/// whichever of the three parts the contract actually carries.
fn codecs(req: &PluginNotificationRequest) -> Option<String> {
    let file = req.media_files.first()?;
    let audio = match (file.audio_codec.as_deref(), file.audio_channels.as_deref()) {
        (Some(codec), Some(channels)) => Some(format!("{codec} {channels}")),
        (Some(codec), None) => Some(codec.to_string()),
        (None, Some(channels)) => Some(channels.to_string()),
        (None, None) => None,
    };
    match (file.video_codec.as_deref(), audio) {
        (Some(video), Some(audio)) => Some(format!("{video} / {audio}")),
        (Some(video), None) => Some(video.to_string()),
        (None, Some(audio)) => Some(audio),
        (None, None) => None,
    }
}

fn release_group(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
        .and_then(|release| release.release_group.clone())
        .or_else(|| {
            req.media_files
                .iter()
                .find_map(|file| file.release_group.clone())
        })
        .map(|group| group.trim().to_string())
        .filter(|group| !group.is_empty())
}

/// Sonarr prefers the release size and falls back to the sum of the imported
/// files (`Discord.cs:317`).
fn total_size_bytes(req: &PluginNotificationRequest) -> Option<i64> {
    if let Some(size) = req
        .download
        .as_ref()
        .and_then(|download| download.size_bytes)
        .filter(|size| *size > 0)
    {
        return Some(size);
    }
    let summed: i64 = req
        .media_files
        .iter()
        .filter_map(|file| file.size_bytes)
        .sum();
    (summed > 0).then_some(summed)
}

fn join_media_languages(
    req: &PluginNotificationRequest,
    pick: impl Fn(&PluginNotificationMediaFile) -> &Vec<String>,
) -> Option<String> {
    let mut values: Vec<String> = Vec::new();
    for file in &req.media_files {
        for value in pick(file) {
            let value = value.trim();
            if !value.is_empty() && !values.iter().any(|existing| existing == value) {
                values.push(value.to_string());
            }
        }
    }
    (!values.is_empty()).then(|| values.join("/"))
}

fn release_title(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
        .and_then(|release| release.source_title.clone())
        .or_else(|| {
            req.media_files
                .iter()
                .find_map(|file| file.scene_name.clone())
        })
        .or_else(|| {
            req.import
                .as_ref()
                .and_then(|import| import.source_title.clone())
        })
        .or_else(|| download_title(req))
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
}

fn download_title(req: &PluginNotificationRequest) -> Option<String> {
    req.download
        .as_ref()
        .and_then(|download| download.title.clone())
        .or_else(|| {
            req.release
                .as_ref()
                .and_then(|release| release.source_title.clone())
        })
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
}

fn indexer(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
        .and_then(|release| {
            release
                .indexer
                .clone()
                .or_else(|| release.source_hint.clone())
        })
        .map(|indexer| indexer.trim().to_string())
        .filter(|indexer| !indexer.is_empty())
}

fn custom_formats(req: &PluginNotificationRequest) -> Option<String> {
    let scores = &req.release.as_ref()?.custom_scores;
    (!scores.is_empty()).then(|| {
        scores
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("|")
    })
}

fn custom_format_score(req: &PluginNotificationRequest) -> Option<i32> {
    let scores = &req.release.as_ref()?.custom_scores;
    (!scores.is_empty()).then(|| scores.values().sum())
}

fn delete_reason(req: &PluginNotificationRequest) -> Option<String> {
    let message = req.summary_message.trim();
    if message.is_empty() {
        Some(event_label(req))
    } else {
        Some(message.to_string())
    }
}

fn deleted_path(req: &PluginNotificationRequest) -> Option<String> {
    if let Some(file) = req.file.as_ref() {
        if let Some(update) = file.media_updates.iter().find(|update| {
            update.update_type == scryer_plugin_sdk::NotificationMediaUpdateType::Deleted
        }) {
            return Some(update.path.clone());
        }
        if let Some(update) = file.media_updates.first() {
            return Some(update.path.clone());
        }
        if let Some(path) = file
            .primary_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            return Some(path.to_string());
        }
    }
    req.media_files.first().map(|file| file.path.clone())
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// `Discord.GetLinksString` (`Discord.cs:690-709`) generalised to Scryer's
/// facets and external-id set.
///
/// Sonarr only ever has a series, so it emits TVDB + Trakt + IMDb. Scryer's
/// title carries a facet and a wider id set, so the series links stay as they
/// are, movies get the TMDB/IMDb pair Sonarr's movie sibling uses, and the anime
/// ids are appended whenever they are present.
fn metadata_links(req: &PluginNotificationRequest) -> Vec<(&'static str, String)> {
    let Some(title) = req.title.as_ref() else {
        return Vec::new();
    };
    let ids = &title.external_ids;
    let facet = title.facet.to_ascii_lowercase();
    let episodic = matches!(facet.as_str(), "series" | "anime" | "tv" | "show");

    let tvdb = first_id(ids.tvdb_id.as_deref(), ids, "tvdb");
    let tmdb = first_id(ids.tmdb_id.as_deref(), ids, "tmdb");
    let imdb = first_id(ids.imdb_id.as_deref(), ids, "imdb");
    let tvmaze = first_id(ids.tvmaze_id.as_deref(), ids, "tvmaze");
    let anidb = first_id(ids.anidb_id.as_deref(), ids, "anidb");
    let anilist = first_id(ids.anilist_ids.first().map(String::as_str), ids, "anilist");
    let mal = first_id(ids.mal_ids.first().map(String::as_str), ids, "mal");
    let kitsu = first_id(ids.kitsu_ids.first().map(String::as_str), ids, "kitsu");

    let mut links: Vec<(&'static str, String)> = Vec::new();
    if episodic {
        if let Some(id) = &tvdb {
            links.push((
                "The TVDB",
                format!("https://thetvdb.com/?tab=series&id={id}"),
            ));
            links.push((
                "Trakt",
                format!("https://trakt.tv/search/tvdb/{id}?id_type=show"),
            ));
        }
        if let Some(id) = &tvmaze {
            links.push(("TVmaze", format!("https://www.tvmaze.com/shows/{id}")));
        }
        if let Some(id) = &tmdb {
            links.push(("TMDB", format!("https://www.themoviedb.org/tv/{id}")));
        }
        if let Some(id) = &imdb {
            links.push(("IMDB", format!("https://imdb.com/title/{id}/")));
        }
    } else {
        if let Some(id) = &tmdb {
            links.push(("TMDB", format!("https://www.themoviedb.org/movie/{id}")));
            links.push((
                "Trakt",
                format!("https://trakt.tv/search/tmdb/{id}?id_type=movie"),
            ));
        }
        if let Some(id) = &imdb {
            links.push(("IMDB", format!("https://imdb.com/title/{id}/")));
        }
        if facet != "movie"
            && let Some(id) = &tvdb
        {
            links.push((
                "The TVDB",
                format!("https://thetvdb.com/?tab=series&id={id}"),
            ));
        }
    }

    // An anime id present is evidence enough; the facet does not have to say so.
    if let Some(id) = &anidb {
        links.push(("AniDB", format!("https://anidb.net/anime/{id}")));
    }
    if let Some(id) = &anilist {
        links.push(("AniList", format!("https://anilist.co/anime/{id}")));
    }
    if let Some(id) = &mal {
        links.push(("MyAnimeList", format!("https://myanimelist.net/anime/{id}")));
    }
    if let Some(id) = &kitsu {
        links.push(("Kitsu", format!("https://kitsu.app/anime/{id}")));
    }

    links
}

fn first_id(
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

fn links_string(req: &PluginNotificationRequest) -> Option<String> {
    let links = metadata_links(req);
    (!links.is_empty()).then(|| {
        links
            .iter()
            .map(|(label, url)| format!("[{label}]({url})"))
            .collect::<Vec<_>>()
            .join(" / ")
    })
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

fn clamp(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_string(), false);
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    (out, true)
}

fn clamp_member(
    parent: &mut Value,
    key: &str,
    limit: usize,
    label: &str,
    warnings: &mut Vec<String>,
) {
    let Some(value) = parent.get_mut(key) else {
        return;
    };
    let Some(text) = value.as_str() else { return };
    let (clamped, truncated) = clamp(text, limit);
    if truncated {
        warnings.push(format!(
            "{label} truncated to Discord's {limit}-character limit"
        ));
        *value = json!(clamped);
    }
}

fn embed_character_total(embed: &Value) -> usize {
    let mut total = 0;
    for key in ["title", "description"] {
        total += embed.get(key).and_then(Value::as_str).map_or(0, char_count);
    }
    total += embed
        .get("author")
        .and_then(|author| author.get("name"))
        .and_then(Value::as_str)
        .map_or(0, char_count);
    total += embed
        .get("footer")
        .and_then(|footer| footer.get("text"))
        .and_then(Value::as_str)
        .map_or(0, char_count);
    if let Some(fields) = embed.get("fields").and_then(Value::as_array) {
        for field in fields {
            for key in ["name", "value"] {
                total += field.get(key).and_then(Value::as_str).map_or(0, char_count);
            }
        }
    }
    total
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// Discord rejects an over-limit message outright, so trim to fit and tell the
/// core what was lost — the addendum's rule for provider limits.
fn enforce_embed_limits(embed: &mut Value, warnings: &mut Vec<String>) {
    clamp_member(embed, "title", EMBED_TITLE_LIMIT, "embed title", warnings);
    clamp_member(
        embed,
        "description",
        EMBED_DESCRIPTION_LIMIT,
        "embed description",
        warnings,
    );
    if let Some(author) = embed.get_mut("author") {
        clamp_member(
            author,
            "name",
            EMBED_AUTHOR_NAME_LIMIT,
            "embed author name",
            warnings,
        );
    }
    if let Some(footer) = embed.get_mut("footer") {
        clamp_member(footer, "text", EMBED_FOOTER_LIMIT, "embed footer", warnings);
    }

    if let Some(fields) = embed.get_mut("fields").and_then(Value::as_array_mut) {
        if fields.len() > EMBED_FIELD_COUNT_LIMIT {
            warnings.push(format!(
                "dropped {} embed field(s) over Discord's {EMBED_FIELD_COUNT_LIMIT}-field limit",
                fields.len() - EMBED_FIELD_COUNT_LIMIT
            ));
            fields.truncate(EMBED_FIELD_COUNT_LIMIT);
        }
        for field in fields.iter_mut() {
            clamp_member(
                field,
                "name",
                EMBED_FIELD_NAME_LIMIT,
                "embed field name",
                warnings,
            );
            clamp_member(
                field,
                "value",
                EMBED_FIELD_VALUE_LIMIT,
                "embed field value",
                warnings,
            );
        }
    }

    if embed_character_total(embed) <= MESSAGE_CHARACTER_LIMIT {
        return;
    }

    let mut dropped = 0usize;
    while embed_character_total(embed) > MESSAGE_CHARACTER_LIMIT {
        let Some(fields) = embed.get_mut("fields").and_then(Value::as_array_mut) else {
            break;
        };
        if fields.pop().is_none() {
            break;
        }
        dropped += 1;
        if fields.is_empty() {
            break;
        }
    }
    if dropped > 0 {
        warnings.push(format!(
            "dropped {dropped} embed field(s) to stay under Discord's {MESSAGE_CHARACTER_LIMIT}-character message limit"
        ));
    }

    let total = embed_character_total(embed);
    if total > MESSAGE_CHARACTER_LIMIT
        && let Some(description) = embed.get("description").and_then(Value::as_str)
    {
        let overflow = total - MESSAGE_CHARACTER_LIMIT;
        let budget = char_count(description).saturating_sub(overflow);
        let (clamped, _) = clamp(description, budget);
        warnings.push(format!(
            "embed description truncated to stay under Discord's {MESSAGE_CHARACTER_LIMIT}-character message limit"
        ));
        embed["description"] = json!(clamped);
    }
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let webhook_url = match required_config("webhook_url") {
        Ok(url) => url,
        Err(error) => return PluginResult::Err(config_error(error)),
    };
    if let Some(error) = validate_webhook_url(&webhook_url) {
        return PluginResult::Err(error);
    }

    let options = RenderOptions::from_config();
    let (payload, warnings) = build_payload(
        req,
        &options,
        config_value("username").as_deref(),
        config_value("avatar").as_deref(),
    );

    let url = with_wait_parameter(&webhook_url);
    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            return PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                "could not encode the Discord webhook payload".to_string(),
                Some(error.to_string()),
            ));
        }
    };

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
            // The host answers a refused or failed egress in-band; report it as
            // a delivery failure rather than a channel misconfiguration.
            let mut failure = error_response(format!("request failed: {error}"), None);
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
    }
}

/// Sonarr validates the webhook URL with `IsValidUrl` (`DiscordSettings.cs:13`).
/// The equivalent here is a typed `InvalidConfig` naming the field.
fn validate_webhook_url(url: &str) -> Option<PluginError> {
    let lowercase = url.to_ascii_lowercase();
    if lowercase.starts_with("https://") || lowercase.starts_with("http://") {
        return None;
    }
    Some(plugin_error(
        PluginErrorCode::InvalidConfig,
        "webhook_url must be an http(s) Discord webhook URL".to_string(),
        Some(format!("configured value: {url}")),
    ))
}

/// `wait=true` makes Discord validate the message before answering and return
/// the created message (`Execute Webhook`, docs.discord.com). Without it a
/// malformed embed is answered with `204` and silently dropped, which is
/// exactly the failure an operator cannot debug. It also yields a message id to
/// report as the delivery id.
fn with_wait_parameter(url: &str) -> String {
    if url.to_ascii_lowercase().contains("wait=") {
        return url.to_string();
    }
    append_query(url, &[("wait", "true".to_string())])
}

fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let detail = discord_detail(body);
    match status {
        200..=299 => {
            let mut response = ok_response();
            response.delivery_id = delivery_id(body);
            response.warnings = warnings;
            PluginResult::Ok(response)
        }
        // The payload this plugin built is wrong; retrying it changes nothing.
        400 => PluginResult::Err(plugin_error(
            PluginErrorCode::Permanent,
            format!("Discord rejected the notification payload: {detail}"),
            Some(format!("HTTP 400: {detail}")),
        )),
        // 401/403 = the token in the URL is not accepted; 404 = the webhook was
        // deleted (`Unknown Webhook`, error code 10015). All three are the
        // operator's `webhook_url`, and Discord's rate-limit guidance is
        // explicit that a 404 webhook must not be retried.
        401 | 403 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("webhook_url was rejected by Discord (HTTP {status}): {detail}"),
            Some(format!("HTTP {status}: {detail}")),
        )),
        404 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "webhook_url no longer exists on Discord (HTTP 404): {detail}. Recreate the webhook and paste the new URL."
            ),
            Some(format!("HTTP 404: {detail}")),
        )),
        429 => {
            let mut failure =
                error_response(format!("HTTP 429: {detail}"), Some("http_429".to_string()));
            failure.retry_after_seconds = retry_after_seconds(headers, body);
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
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

fn delivery_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

/// Discord's 429 body carries `retry_after` in **seconds** (a float); the
/// `Retry-After` and `X-RateLimit-Reset-After` headers say the same thing.
fn retry_after_seconds(headers: &BTreeMap<String, String>, body: &[u8]) -> Option<i64> {
    let from_body = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("retry_after").and_then(Value::as_f64));
    let seconds = from_body
        .or_else(|| header(headers, "retry-after").and_then(|value| value.parse::<f64>().ok()))
        .or_else(|| {
            header(headers, "x-ratelimit-reset-after").and_then(|value| value.parse::<f64>().ok())
        })?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds.ceil() as i64).max(1))
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Discord's JSON error shape is `{ "message": …, "code": … }`
/// (docs.discord.com, "Opcodes and Status Codes").
fn discord_detail(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
        let message = map
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty());
        if let Some(message) = message {
            return match map.get("code").and_then(Value::as_i64) {
                Some(code) => format!("{message} (code {code})"),
                None => message.to_string(),
            };
        }
    }
    if text.is_empty() {
        "no response body".to_string()
    } else {
        clamp(text, 500).0
    }
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

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// `Discord.BytesToString` (`Discord.cs:676-688`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        NotificationMediaUpdateType, PluginNotificationApp, PluginNotificationApplicationUpdate,
        PluginNotificationDownload, PluginNotificationExternalIds, PluginNotificationFile,
        PluginNotificationHealth, PluginNotificationImport, PluginNotificationMediaUpdate,
        PluginNotificationRelease, PluginNotificationTitle,
    };

    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: None,
            occurred_at: None,
            correlation_id: None,
            actor: None,
            severity: None,
            is_test: event_type == NotificationEventType::Test,
            summary_title: "Summary".to_string(),
            summary_message: "Summary message.".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "0.0.0-test".to_string(),
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
            name: "Cinder Line".to_string(),
            facet: "series".to_string(),
            year: Some(2019),
            slug: None,
            path: Some("/media/TV/Cinder Line".to_string()),
            overview: Some("A show about a line.".to_string()),
            sort_title: None,
            background_url: Some("https://images.test/fanart.jpg".to_string()),
            poster_url: Some("https://images.test/poster.jpg".to_string()),
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds {
                tvdb_id: Some("12345".to_string()),
                imdb_id: Some("tt0999".to_string()),
                ..Default::default()
            },
        }
    }

    fn episode(season: &str, number: &str, title: &str) -> PluginNotificationEpisode {
        PluginNotificationEpisode {
            season_number: Some(season.to_string()),
            episode_number: Some(number.to_string()),
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    /// Everything the contract can carry, so every field option has data.
    fn fully_populated_grab() -> PluginNotificationRequest {
        let mut req = request(NotificationEventType::Grab);
        req.occurred_at = Some("2026-09-01T10:00:00Z".to_string());
        req.summary_title = "Grabbed: Cinder Line".to_string();
        req.summary_message = "Grabbed 'Cinder.Line.S02E03' for 'Cinder Line'.".to_string();
        req.title = Some(series_title());
        req.episode = Some(PluginNotificationEpisode {
            overview: Some("The one with the line.".to_string()),
            ..episode("2", "3", "Trackside")
        });
        req.episodes = vec![episode("2", "3", "Trackside")];
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Cinder.Line.S02E03.1080p.WEB-DL".to_string()),
            quality: Some("WEBDL-1080p".to_string()),
            release_group: Some("SCRY".to_string()),
            indexer: Some("NZBGeek".to_string()),
            custom_scores: [("HDR".to_string(), 50), ("Repack".to_string(), 5)]
                .into_iter()
                .collect(),
            ..Default::default()
        });
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            size_bytes: Some(2_147_483_648),
            title: Some("Cinder.Line.S02E03.1080p.WEB-DL".to_string()),
            ..Default::default()
        });
        req
    }

    fn embed_of(payload: &Value) -> &Value {
        &payload["embeds"][0]
    }

    fn field_named<'a>(embed: &'a Value, name: &str) -> Option<&'a Value> {
        embed
            .get("fields")?
            .as_array()?
            .iter()
            .find(|field| field["name"] == name)
    }

    fn render(req: &PluginNotificationRequest) -> (Value, Vec<String>) {
        build_payload(req, &RenderOptions::defaults(), None, None)
    }

    // -- descriptor ---------------------------------------------------------

    #[test]
    fn descriptor_carries_sonarrs_three_field_sets_and_event_filters() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = descriptor.provider else {
            panic!("expected a notification provider");
        };
        assert_eq!(notification.provider_type, "discord");
        assert!(
            notification
                .capabilities
                .event_options
                .supports_upgrade_filter
        );
        assert!(
            notification
                .capabilities
                .event_options
                .supports_delete_for_upgrade_filter
        );
        assert!(
            notification
                .capabilities
                .event_options
                .supports_health_warning_filter
        );

        for key in ["grab_fields", "import_fields", "manual_interaction_fields"] {
            let field = notification
                .config_fields
                .iter()
                .find(|field| field.key == key)
                .unwrap_or_else(|| panic!("{key} is missing"));
            assert_eq!(field.field_type, ConfigFieldType::Tag);
            assert!(
                !field.options.is_empty(),
                "{key} must offer Sonarr's options"
            );
            assert!(field.default_value.is_some(), "{key} must carry a default");
        }

        // Existing keys are a public contract and must not be renamed.
        for key in ["webhook_url", "username", "avatar", "author"] {
            assert!(
                notification
                    .config_fields
                    .iter()
                    .any(|field| field.key == key),
                "{key} must stay in the descriptor"
            );
        }
    }

    #[test]
    fn import_defaults_leave_custom_formats_opt_in() {
        // DiscordSettings.cs:37-52 ships Import without the two custom-format
        // options.
        assert!(!IMPORT_FIELD_DEFAULTS.contains(&"custom_formats"));
        assert!(!IMPORT_FIELD_DEFAULTS.contains(&"custom_format_score"));
        assert!(
            IMPORT_FIELD_OPTIONS
                .iter()
                .any(|(v, _)| *v == "custom_formats")
        );
    }

    #[test]
    fn an_absent_field_set_falls_back_to_defaults_but_an_empty_one_renders_nothing() {
        assert_eq!(parse_field_set(""), Vec::<String>::new());
        assert_eq!(
            parse_field_set("Quality, links ;release"),
            vec![
                "quality".to_string(),
                "links".to_string(),
                "release".to_string()
            ]
        );
    }

    // -- heading and links --------------------------------------------------

    #[test]
    fn heading_follows_sonarrs_series_episode_format() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.episodes = vec![episode("2", "3", "Trackside"), episode("2", "4", "Signal")];
        assert_eq!(heading(&req), "Cinder Line - 2x03x04 - Trackside + Signal");
    }

    #[test]
    fn heading_prefers_the_contracts_rendered_episode_display() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.episode = Some(PluginNotificationEpisode {
            display: Some("S02E03 - Trackside".to_string()),
            ..Default::default()
        });
        assert_eq!(heading(&req), "Cinder Line - S02E03 - Trackside");
    }

    #[test]
    fn heading_uses_the_air_date_for_a_daily_episode() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.episodes = vec![PluginNotificationEpisode {
            air_date: Some("2026-09-01".to_string()),
            title: Some("Monday".to_string()),
            ..Default::default()
        }];
        assert_eq!(heading(&req), "Cinder Line - 2026-09-01 - Monday");
    }

    #[test]
    fn heading_escapes_backticks_and_caps_at_the_embed_title_limit() {
        let mut req = request(NotificationEventType::Grab);
        let mut title = series_title();
        title.name = format!("`{}", "a".repeat(400));
        req.title = Some(title);
        let heading = heading(&req);
        assert!(heading.starts_with("\\`"));
        assert_eq!(heading.chars().count(), EMBED_TITLE_LIMIT);
        assert!(heading.ends_with("..."));
    }

    #[test]
    fn heading_falls_back_to_the_summary_without_a_title_block() {
        let req = request(NotificationEventType::Grab);
        assert_eq!(heading(&req), "Summary");
    }

    #[test]
    fn links_are_facet_aware() {
        let mut series = request(NotificationEventType::Grab);
        series.title = Some(series_title());
        let rendered = links_string(&series).expect("series links");
        assert!(rendered.contains("[The TVDB](https://thetvdb.com/?tab=series&id=12345)"));
        assert!(rendered.contains("[Trakt](https://trakt.tv/search/tvdb/12345?id_type=show)"));
        assert!(rendered.contains("[IMDB](https://imdb.com/title/tt0999/)"));

        let mut movie = request(NotificationEventType::Grab);
        movie.title = Some(PluginNotificationTitle {
            facet: "movie".to_string(),
            external_ids: PluginNotificationExternalIds {
                tmdb_id: Some("603".to_string()),
                imdb_id: Some("tt0133093".to_string()),
                ..Default::default()
            },
            ..series_title()
        });
        let rendered = links_string(&movie).expect("movie links");
        assert!(rendered.contains("[TMDB](https://www.themoviedb.org/movie/603)"));
        assert!(rendered.contains("[Trakt](https://trakt.tv/search/tmdb/603?id_type=movie)"));
        assert!(!rendered.contains("thetvdb.com"));
    }

    #[test]
    fn anime_ids_render_from_the_typed_fields_and_from_by_source() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(PluginNotificationTitle {
            facet: "anime".to_string(),
            external_ids: PluginNotificationExternalIds {
                anidb_id: Some("4321".to_string()),
                by_source: [("mal".to_string(), vec!["999".to_string()])]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            ..series_title()
        });
        let rendered = links_string(&req).expect("anime links");
        assert!(rendered.contains("[AniDB](https://anidb.net/anime/4321)"));
        assert!(rendered.contains("[MyAnimeList](https://myanimelist.net/anime/999)"));
    }

    #[test]
    fn the_embed_url_is_the_primary_metadata_link() {
        let req = fully_populated_grab();
        let (payload, _) = render(&req);
        assert_eq!(
            embed_of(&payload)["url"],
            json!("https://thetvdb.com/?tab=series&id=12345")
        );
    }

    // -- per-event rendering ------------------------------------------------

    #[test]
    fn a_grab_renders_sonarrs_grab_field_set() {
        let req = fully_populated_grab();
        let (payload, warnings) = render(&req);
        let embed = embed_of(&payload);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(embed["title"], json!("Cinder Line - 2x03 - Trackside"));
        assert_eq!(
            embed["description"],
            json!("Episode Grabbed\nGrabbed 'Cinder.Line.S02E03' for 'Cinder Line'.")
        );
        assert_eq!(embed["color"], json!(COLOR_STANDARD));
        assert_eq!(embed["author"]["name"], json!("Scryer"));
        assert!(embed["author"].get("icon_url").is_none());
        assert_eq!(embed["timestamp"], json!("2026-09-01T10:00:00Z"));
        assert_eq!(
            embed["thumbnail"]["url"],
            json!("https://images.test/poster.jpg")
        );
        assert_eq!(
            embed["image"]["url"],
            json!("https://images.test/fanart.jpg")
        );

        assert_eq!(
            field_named(embed, "Overview").unwrap()["value"],
            json!("The one with the line.")
        );
        assert_eq!(
            field_named(embed, "Quality").unwrap()["value"],
            json!("WEBDL-1080p")
        );
        assert_eq!(
            field_named(embed, "Quality").unwrap()["inline"],
            json!(true)
        );
        assert_eq!(field_named(embed, "Group").unwrap()["value"], json!("SCRY"));
        assert_eq!(field_named(embed, "Size").unwrap()["value"], json!("2 GB"));
        assert_eq!(
            field_named(embed, "Release").unwrap()["value"],
            json!("```Cinder.Line.S02E03.1080p.WEB-DL```")
        );
        assert_eq!(
            field_named(embed, "Indexer").unwrap()["value"],
            json!("NZBGeek")
        );
        assert_eq!(
            field_named(embed, "Custom Formats").unwrap()["value"],
            json!("HDR|Repack")
        );
        assert_eq!(
            field_named(embed, "Custom Format Score").unwrap()["value"],
            json!("55")
        );
        assert!(field_named(embed, "Links").is_some());
        // No contract carrier for these two.
        assert!(field_named(embed, "Rating").is_none());
        assert!(field_named(embed, "Genres").is_none());
    }

    #[test]
    fn the_grab_field_order_is_the_operators_order() {
        let req = fully_populated_grab();
        let options = RenderOptions {
            grab_fields: vec!["indexer".to_string(), "quality".to_string()],
            ..RenderOptions::defaults()
        };
        let (payload, _) = build_payload(&req, &options, None, None);
        let fields = embed_of(&payload)["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["name"], json!("Indexer"));
        assert_eq!(fields[1]["name"], json!("Quality"));
        assert!(embed_of(&payload).get("thumbnail").is_none());
    }

    #[test]
    fn the_sparse_shape_the_core_sends_today_still_renders() {
        // ImportComplete as dispatcher.rs actually builds it: title, episode
        // ids, release source title and quality, download client, import block.
        let mut req = request(NotificationEventType::ImportComplete);
        req.summary_title = "Import complete: Cinder Line".to_string();
        req.summary_message = "Imported 2 files for 'Cinder Line'.".to_string();
        req.title = Some(PluginNotificationTitle {
            overview: None,
            background_url: None,
            ..series_title()
        });
        req.episode = Some(PluginNotificationEpisode {
            episode_ids: vec!["ep-1".to_string(), "ep-2".to_string()],
            ..Default::default()
        });
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Cinder.Line.S02.1080p".to_string()),
            quality: Some("WEBDL-1080p".to_string()),
            ..Default::default()
        });
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            ..Default::default()
        });
        req.import = Some(PluginNotificationImport {
            imported_count: Some(2),
            ..Default::default()
        });

        let (payload, warnings) = render(&req);
        let embed = embed_of(&payload);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(embed["title"], json!("Cinder Line"));
        assert_eq!(
            embed["description"],
            json!("Import Complete\nImported 2 files for 'Cinder Line'.")
        );
        assert_eq!(embed["color"], json!(COLOR_SUCCESS));
        assert!(embed.get("timestamp").is_none(), "no clock, no timestamp");
        assert!(field_named(embed, "Codecs").is_none());
        assert!(field_named(embed, "Languages").is_none());
        assert_eq!(
            field_named(embed, "Release").unwrap()["value"],
            json!("```Cinder.Line.S02.1080p```")
        );
    }

    #[test]
    fn an_import_renders_codecs_languages_and_subtitles_from_media_files() {
        let mut req = request(NotificationEventType::Upgrade);
        req.title = Some(series_title());
        req.media_files = vec![PluginNotificationMediaFile {
            path: "/media/TV/Cinder Line/S02E03.mkv".to_string(),
            size_bytes: Some(1_073_741_824),
            video_codec: Some("x265".to_string()),
            audio_codec: Some("EAC3".to_string()),
            audio_channels: Some("5.1".to_string()),
            audio_languages: vec!["English".to_string(), "Japanese".to_string()],
            subtitle_languages: vec!["English".to_string()],
            scene_name: Some("Cinder.Line.S02E03.SCENE".to_string()),
            quality: Some("Bluray-1080p".to_string()),
            release_group: Some("GRP".to_string()),
            ..Default::default()
        }];

        let (payload, _) = render(&req);
        let embed = embed_of(&payload);
        assert_eq!(embed["color"], json!(COLOR_UPGRADE));
        assert_eq!(
            field_named(embed, "Codecs").unwrap()["value"],
            json!("x265 / EAC3 5.1")
        );
        assert_eq!(
            field_named(embed, "Languages").unwrap()["value"],
            json!("English/Japanese")
        );
        assert_eq!(
            field_named(embed, "Subtitles").unwrap()["value"],
            json!("English")
        );
        assert_eq!(field_named(embed, "Size").unwrap()["value"], json!("1 GB"));
        assert_eq!(
            field_named(embed, "Quality").unwrap()["value"],
            json!("Bluray-1080p")
        );
    }

    #[test]
    fn a_download_failure_is_not_rendered_as_an_import() {
        // dispatcher.rs maps DownloadFailed onto NotificationEventType::Download.
        let mut req = request(NotificationEventType::Download);
        req.summary_title = "Download failed: Cinder Line".to_string();
        req.summary_message = "The download client reported a failure.".to_string();
        req.title = Some(series_title());
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            ..Default::default()
        });

        let (payload, _) = render(&req);
        let embed = embed_of(&payload);
        assert_eq!(embed["color"], json!(COLOR_DANGER));
        assert!(
            embed["description"]
                .as_str()
                .unwrap()
                .starts_with("Download Failed")
        );
    }

    #[test]
    fn a_successful_download_is_an_import_and_an_upgrade_flag_recolours_it() {
        let mut req = request(NotificationEventType::Download);
        req.summary_title = "Imported: Cinder Line".to_string();
        req.title = Some(series_title());
        let (payload, _) = render(&req);
        assert_eq!(embed_of(&payload)["color"], json!(COLOR_SUCCESS));
        assert!(
            embed_of(&payload)["description"]
                .as_str()
                .unwrap()
                .starts_with("Episode Imported")
        );

        req.import = Some(PluginNotificationImport {
            upgrade: true,
            ..Default::default()
        });
        let (payload, _) = render(&req);
        assert_eq!(embed_of(&payload)["color"], json!(COLOR_UPGRADE));
        assert!(
            embed_of(&payload)["description"]
                .as_str()
                .unwrap()
                .starts_with("Episode Upgraded")
        );
    }

    #[test]
    fn a_movie_facet_gets_neutral_event_wording() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(PluginNotificationTitle {
            facet: "movie".to_string(),
            ..series_title()
        });
        assert_eq!(event_label(&req), "Grabbed");
        req.event_type = NotificationEventType::TitleAdded;
        assert_eq!(event_label(&req), "Added");
    }

    #[test]
    fn a_rename_posts_the_word_renamed_with_a_title_embed() {
        let mut req = request(NotificationEventType::Rename);
        req.title = Some(series_title());
        let (payload, _) = render(&req);
        assert_eq!(payload["content"], json!("Renamed"));
        let embed = embed_of(&payload);
        assert_eq!(embed["title"], json!("Cinder Line"));
        assert!(embed.get("description").is_none());
        assert!(embed.get("fields").is_none());
    }

    #[test]
    fn a_file_delete_renders_reason_and_file_name_in_danger_colour() {
        let mut req = request(NotificationEventType::FileDeleted);
        req.title = Some(series_title());
        req.summary_message = "Deleted because the file was missing on disk.".to_string();
        req.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Cinder Line/S02E03.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });

        let (payload, _) = render(&req);
        let embed = embed_of(&payload);
        assert_eq!(embed["color"], json!(COLOR_DANGER));
        assert_eq!(
            field_named(embed, "Reason").unwrap()["value"],
            json!("Deleted because the file was missing on disk.")
        );
        assert_eq!(
            field_named(embed, "File name").unwrap()["value"],
            json!("```/media/TV/Cinder Line/S02E03.mkv```")
        );
    }

    #[test]
    fn a_title_delete_uses_the_summary_as_the_description() {
        let mut req = request(NotificationEventType::TitleDeleted);
        req.title = Some(series_title());
        req.summary_message = "Deleted 'Cinder Line' from Scryer.".to_string();
        let (payload, _) = render(&req);
        let embed = embed_of(&payload);
        assert_eq!(
            embed["description"],
            json!("Deleted 'Cinder Line' from Scryer.")
        );
        assert_eq!(embed["color"], json!(COLOR_DANGER));
        // Sonarr's series-delete embed carries exactly one field.
        assert_eq!(embed["fields"].as_array().unwrap().len(), 1);
        assert!(field_named(embed, "Links").is_some());
    }

    #[test]
    fn health_events_render_the_source_and_message() {
        let mut issue = request(NotificationEventType::HealthIssue);
        issue.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable due to failures".to_string()),
            ..Default::default()
        });
        let (payload, _) = render(&issue);
        let embed = embed_of(&payload);
        assert_eq!(embed["title"], json!("IndexerStatusCheck"));
        assert_eq!(
            embed["description"],
            json!("Indexers unavailable due to failures")
        );
        assert_eq!(embed["color"], json!(COLOR_WARNING));

        let mut restored = request(NotificationEventType::HealthRestored);
        restored.health = issue.health.clone();
        let (payload, _) = render(&restored);
        let embed = embed_of(&payload);
        assert_eq!(
            embed["title"],
            json!("Health Issue Resolved: IndexerStatusCheck")
        );
        assert_eq!(
            embed["description"],
            json!("The following issue is now resolved: Indexers unavailable due to failures")
        );
        assert_eq!(embed["color"], json!(COLOR_SUCCESS));
    }

    #[test]
    fn an_application_update_renders_the_two_version_fields() {
        let mut req = request(NotificationEventType::ApplicationUpdate);
        req.summary_title = "Application Updated".to_string();
        req.summary_message = String::new();
        req.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            ..Default::default()
        });
        let (payload, _) = render(&req);
        let embed = embed_of(&payload);
        assert_eq!(embed["title"], json!("Application Updated"));
        assert_eq!(embed["color"], json!(COLOR_STANDARD));
        assert_eq!(
            field_named(embed, "Previous Version").unwrap()["value"],
            json!("0.19.7")
        );
        assert_eq!(
            field_named(embed, "New Version").unwrap()["value"],
            json!("0.19.8")
        );
    }

    #[test]
    fn manual_interaction_renders_the_download_title_and_size() {
        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.title = Some(series_title());
        req.download = Some(PluginNotificationDownload {
            title: Some("Cinder.Line.S02E03.WEB".to_string()),
            size_bytes: Some(536_870_912),
            ..Default::default()
        });
        let (payload, _) = render(&req);
        let embed = embed_of(&payload);
        assert_eq!(embed["color"], json!(COLOR_STANDARD));
        assert!(
            embed["description"]
                .as_str()
                .unwrap()
                .starts_with("Manual interaction needed")
        );
        assert_eq!(
            field_named(embed, "Download").unwrap()["value"],
            json!("```Cinder.Line.S02E03.WEB```")
        );
        assert_eq!(
            field_named(embed, "Size").unwrap()["value"],
            json!("512 MB")
        );
    }

    #[test]
    fn a_scryer_only_event_renders_generically_rather_than_failing() {
        let mut req = request(NotificationEventType::SubtitleDownloaded);
        req.title = Some(series_title());
        req.release = Some(PluginNotificationRelease {
            quality: Some("WEBDL-1080p".to_string()),
            ..Default::default()
        });
        let (payload, _) = render(&req);
        let embed = embed_of(&payload);
        assert_eq!(embed["color"], json!(COLOR_SUCCESS));
        assert!(
            embed["description"]
                .as_str()
                .unwrap()
                .starts_with("Subtitle Downloaded")
        );
        assert!(field_named(embed, "Quality").is_some());
    }

    #[test]
    fn severity_overrides_the_event_colour_but_a_warning_never_downgrades_danger() {
        let mut req = request(NotificationEventType::Grab);
        req.severity = Some(NotificationSeverity::Warning);
        assert_eq!(embed_color(&req), COLOR_WARNING);
        req.severity = Some(NotificationSeverity::Error);
        assert_eq!(embed_color(&req), COLOR_DANGER);

        let mut deleted = request(NotificationEventType::FileDeleted);
        deleted.severity = Some(NotificationSeverity::Warning);
        assert_eq!(embed_color(&deleted), COLOR_DANGER);
    }

    // -- test message -------------------------------------------------------

    #[test]
    fn a_test_request_posts_plain_content_with_no_embed() {
        let mut req = request(NotificationEventType::Test);
        req.occurred_at = Some("2026-09-01T10:00:00Z".to_string());
        let (payload, _) = render(&req);
        assert_eq!(
            payload["content"],
            json!("Test message from Scryer posted at 2026-09-01T10:00:00Z")
        );
        assert!(payload.get("embeds").is_none());

        req.occurred_at = None;
        let (payload, _) = render(&req);
        assert_eq!(payload["content"], json!("Test message from Scryer"));
    }

    #[test]
    fn username_and_avatar_overrides_match_create_payload() {
        let req = fully_populated_grab();
        let (payload, _) = build_payload(
            &req,
            &RenderOptions::defaults(),
            Some("Scryer Bot"),
            Some("https://images.test/avatar.png"),
        );
        assert_eq!(payload["username"], json!("Scryer Bot"));
        assert_eq!(
            payload["avatar_url"],
            json!("https://images.test/avatar.png")
        );

        let (payload, _) = build_payload(&req, &RenderOptions::defaults(), Some("  "), None);
        assert!(payload.get("username").is_none());
        assert!(payload.get("avatar_url").is_none());
    }

    #[test]
    fn a_configured_author_and_icon_replace_the_app_name() {
        let req = fully_populated_grab();
        let options = RenderOptions {
            author: Some("Media Team".to_string()),
            author_icon_url: Some("https://images.test/icon.png".to_string()),
            ..RenderOptions::defaults()
        };
        let (payload, _) = build_payload(&req, &options, None, None);
        let author = &embed_of(&payload)["author"];
        assert_eq!(author["name"], json!("Media Team"));
        assert_eq!(author["icon_url"], json!("https://images.test/icon.png"));
    }

    // -- limits -------------------------------------------------------------

    #[test]
    fn an_over_long_description_is_truncated_with_a_warning() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.summary_message = "x".repeat(EMBED_DESCRIPTION_LIMIT + 500);
        let (payload, warnings) = render(&req);
        let description = embed_of(&payload)["description"].as_str().unwrap();
        assert_eq!(description.chars().count(), EMBED_DESCRIPTION_LIMIT);
        assert!(description.ends_with('…'));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("embed description")),
            "{warnings:?}"
        );
    }

    #[test]
    fn field_values_are_clamped_to_the_field_limit() {
        let mut embed = json!({
            "fields": [{ "name": "Release", "value": "y".repeat(EMBED_FIELD_VALUE_LIMIT + 10), "inline": false }]
        });
        let mut warnings = Vec::new();
        enforce_embed_limits(&mut embed, &mut warnings);
        assert_eq!(
            embed["fields"][0]["value"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            EMBED_FIELD_VALUE_LIMIT
        );
        assert!(warnings.iter().any(|w| w.contains("embed field value")));
    }

    #[test]
    fn more_than_twenty_five_fields_are_dropped_with_a_warning() {
        let fields: Vec<Value> = (0..30)
            .map(|index| json!({ "name": format!("f{index}"), "value": "v", "inline": false }))
            .collect();
        let mut embed = json!({ "fields": fields });
        let mut warnings = Vec::new();
        enforce_embed_limits(&mut embed, &mut warnings);
        assert_eq!(
            embed["fields"].as_array().unwrap().len(),
            EMBED_FIELD_COUNT_LIMIT
        );
        assert!(warnings.iter().any(|w| w.contains("25-field limit")));
    }

    #[test]
    fn the_six_thousand_character_budget_drops_fields_before_the_description() {
        let fields: Vec<Value> = (0..10)
            .map(|index| {
                json!({
                    "name": format!("f{index}"),
                    "value": "z".repeat(EMBED_FIELD_VALUE_LIMIT),
                    "inline": false
                })
            })
            .collect();
        let mut embed = json!({
            "title": "t",
            "description": "d".repeat(1000),
            "fields": fields
        });
        let mut warnings = Vec::new();
        enforce_embed_limits(&mut embed, &mut warnings);
        assert!(embed_character_total(&embed) <= MESSAGE_CHARACTER_LIMIT);
        assert!(embed["fields"].as_array().unwrap().len() < 10);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("6000-character message limit"))
        );
    }

    // -- delivery classification -------------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn a_204_is_a_successful_delivery() {
        let result = classify_response(204, &BTreeMap::new(), b"", vec!["warned".to_string()]);
        let PluginResult::Ok(response) = result else {
            panic!("204 must be a successful delivery");
        };
        assert!(response.success);
        assert_eq!(response.warnings, vec!["warned".to_string()]);
        assert!(response.delivery_id.is_none());
    }

    #[test]
    fn a_200_with_wait_true_reports_the_message_id_as_the_delivery_id() {
        let result = classify_response(
            200,
            &BTreeMap::new(),
            br#"{"id":"1234567890","channel_id":"42"}"#,
            Vec::new(),
        );
        let PluginResult::Ok(response) = result else {
            panic!("200 must be a successful delivery");
        };
        assert!(response.success);
        assert_eq!(response.delivery_id.as_deref(), Some("1234567890"));
    }

    #[test]
    fn a_400_is_a_permanent_typed_error_carrying_discords_own_message() {
        let result = classify_response(
            400,
            &BTreeMap::new(),
            br#"{"message":"Invalid Form Body","code":50035}"#,
            Vec::new(),
        );
        let PluginResult::Err(error) = result else {
            panic!("400 must be a typed error");
        };
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("Invalid Form Body"));
        assert!(error.public_message.contains("50035"));
    }

    #[test]
    fn an_unusable_webhook_is_an_invalid_config_naming_the_field() {
        for status in [401u16, 403, 404] {
            let result = classify_response(
                status,
                &BTreeMap::new(),
                br#"{"message":"Unknown Webhook","code":10015}"#,
                Vec::new(),
            );
            let PluginResult::Err(error) = result else {
                panic!("HTTP {status} must be a typed error");
            };
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(
                error.public_message.contains("webhook_url"),
                "HTTP {status}: {}",
                error.public_message
            );
        }
    }

    #[test]
    fn a_429_is_a_delivery_failure_carrying_discords_retry_after() {
        let result = classify_response(
            429,
            &headers(&[("retry-after", "9")]),
            br#"{"message":"You are being rate limited.","retry_after":1.483,"global":false}"#,
            Vec::new(),
        );
        let PluginResult::Ok(response) = result else {
            panic!("429 must stay a delivery failure");
        };
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));
        // The body's float wins over the header, rounded up to whole seconds.
        assert_eq!(response.retry_after_seconds, Some(2));
    }

    #[test]
    fn a_429_without_a_body_falls_back_to_the_rate_limit_headers() {
        let result = classify_response(
            429,
            &headers(&[("X-RateLimit-Reset-After", "0.25")]),
            b"",
            Vec::new(),
        );
        let PluginResult::Ok(response) = result else {
            panic!("429 must stay a delivery failure");
        };
        assert_eq!(response.retry_after_seconds, Some(1));
    }

    #[test]
    fn a_5xx_is_a_reported_delivery_failure_not_a_typed_error() {
        let result = classify_response(503, &BTreeMap::new(), b"upstream exploded", Vec::new());
        let PluginResult::Ok(response) = result else {
            panic!("a 5xx must stay in-band as a delivery failure");
        };
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_503"));
        assert!(response.error.unwrap().contains("upstream exploded"));
    }

    #[test]
    fn a_non_http_webhook_url_is_rejected_before_any_request() {
        let error = validate_webhook_url("discord.com/api/webhooks/1/token")
            .expect("a schemeless URL must be rejected");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("webhook_url"));
        assert!(validate_webhook_url("https://discord.com/api/webhooks/1/token").is_none());
        // The legacy host still works and must not be rejected.
        assert!(validate_webhook_url("https://discordapp.com/api/webhooks/1/token").is_none());
    }

    #[test]
    fn wait_true_is_appended_once_and_never_overrides_an_explicit_choice() {
        assert_eq!(
            with_wait_parameter("https://discord.com/api/webhooks/1/token"),
            "https://discord.com/api/webhooks/1/token?wait=true"
        );
        assert_eq!(
            with_wait_parameter("https://discord.com/api/webhooks/1/token?thread_id=5"),
            "https://discord.com/api/webhooks/1/token?thread_id=5&wait=true"
        );
        assert_eq!(
            with_wait_parameter("https://discord.com/api/webhooks/1/token?wait=false"),
            "https://discord.com/api/webhooks/1/token?wait=false"
        );
    }

    #[test]
    fn byte_formatting_matches_sonarrs_table() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(1_610_612_736), "1.5 GB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
    }

    #[test]
    fn the_payload_is_valid_json_for_every_event_type() {
        for event_type in general_notification_events() {
            let mut req = request(event_type);
            req.title = Some(series_title());
            let (payload, _) = render(&req);
            let encoded = serde_json::to_string(&payload).expect("payload encodes");
            assert!(
                encoded.contains("content") || encoded.contains("embeds"),
                "{event_type:?} produced an empty payload"
            );
        }
    }
}
