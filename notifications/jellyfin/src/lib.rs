//! Jellyfin media-server notifications, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:notification/notification@1.0.0`: two exports
//! carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`, `process`
//! exchanges a `PluginCommandRequest` for a `PluginCommandResponse`), plus the
//! shared `scryer:host/services@1.0.0` import that carries config and HTTP.
//!
//! The request planner, the path mapping and the Jellyfin API calls are
//! untouched. What changed is the transport: the two exported entry points
//! collapse into one `process` export dispatching the SDK's
//! `PluginNotificationCommand`, and `config::get` / `http::request` reach
//! Scryer through [`scryer_plugin_pdk`] rather than the removed core-module
//! host ABI. The response helpers stop hand-serializing a `PluginResult` — the
//! world's dispatch owns that envelope now, so they return the typed value.
//!
//! Every failure is a typed `PluginResult::Err`: a missing setting or bad
//! `path_mappings` is `InvalidConfig`, a rejected API key `invalid_api_key`, a
//! 404 `endpoint_not_found`, 429 `RateLimited`, 5xx `UpstreamUnavailable`, an
//! unanswered request the PDK's transport reading, and a notification the
//! plugin cannot turn into a Jellyfin call `Permanent`.

mod redact;

use std::collections::HashSet;

use scryer_plugin_pdk::sdk::command::{PluginNotificationCommand, PluginNotificationCommandResult};
use scryer_plugin_pdk::{HttpRequest, codes, config, http};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, NotificationCapabilities, NotificationDeliveryMode,
    NotificationDescriptor, NotificationEventType as SdkNotificationEventType,
    NotificationPayloadFormat, PluginDescriptor, PluginError, PluginErrorCode,
    PluginNotificationFile, PluginNotificationRequest, PluginNotificationResponse,
    PluginNotificationTitle, PluginResult, ProviderDescriptor, SDK_VERSION,
};
use serde::Serialize;

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:notification/notification@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/notification-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = default_descriptor,
    handler = handle_notification_command,
);

#[derive(Debug, Clone)]
struct JellyfinConfig {
    base_url: String,
    api_key: String,
    path_mappings: String,
    /// Opt-in: push Scryer artwork and title metadata onto the Jellyfin item
    /// after the library refresh. Off by default so existing channels keep
    /// sending only refresh requests.
    export_images_metadata: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathMapping {
    source_prefix: String,
    source_prefix_normalized: String,
    destination_prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MediaUpdateType {
    Created,
    Modified,
    Deleted,
}

impl MediaUpdateType {
    fn as_jellyfin(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Modified => "Modified",
            Self::Deleted => "Deleted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MediaUpdate {
    path: String,
    update_type: MediaUpdateType,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExternalIds {
    tmdb_id: Option<String>,
    imdb_id: Option<String>,
    tvdb_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JellyfinRequestPlan {
    SystemInfo,
    MediaUpdated {
        updates: Vec<MediaUpdate>,
    },
    MoviesUpdated {
        tmdb_id: Option<String>,
        imdb_id: Option<String>,
    },
    SeriesUpdated {
        tvdb_id: String,
    },
    /// Resolve the Jellyfin item id from one of Scryer's provider ids.
    ItemLookup {
        provider: String,
        id: String,
    },
    /// Ask Jellyfin to download one of Scryer's artwork URLs into its own image store.
    DownloadRemoteImage {
        item_id: String,
        image_type: &'static str,
        image_url: String,
    },
    /// Set the title fields Scryer already knows on the Jellyfin item.
    UpdateItemMetadata {
        item_id: String,
        fields: ItemMetadataFields,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ItemMetadataFields {
    overview: Option<String>,
    tags: Vec<String>,
    release_date: Option<String>,
    runtime_minutes: Option<i32>,
    studio: Option<String>,
    network: Option<String>,
    content_status: Option<String>,
    sort_title: Option<String>,
}

impl ItemMetadataFields {
    /// Only fields Scryer actually has are sent; Jellyfin keeps the rest.
    fn from_title(title: Option<&PluginNotificationTitle>) -> Option<Self> {
        let title = title?;
        let fields = Self {
            overview: title.overview.clone(),
            tags: title.tags.clone(),
            release_date: title.release_date.clone(),
            runtime_minutes: title.runtime_minutes,
            studio: title.studio.clone(),
            network: title.network.clone(),
            content_status: title.content_status.clone(),
            sort_title: title.sort_title.clone(),
        };
        (fields.overview.is_some()
            || !fields.tags.is_empty()
            || fields.release_date.is_some()
            || fields.runtime_minutes.is_some()
            || fields.studio.is_some()
            || fields.network.is_some()
            || fields.content_status.is_some()
            || fields.sort_title.is_some())
        .then_some(fields)
    }
}

impl JellyfinRequestPlan {
    /// Whether this call refreshes a title by provider id rather than by path.
    /// Jellyfin serves these endpoints; Silo's Jellyfin-compatible API does not.
    fn is_provider_id_refresh(&self) -> bool {
        matches!(
            self,
            Self::MoviesUpdated { .. } | Self::SeriesUpdated { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedHttpRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl PreparedHttpRequest {
    fn new(method: &str, url: String) -> Self {
        Self {
            method: method.to_string(),
            url,
            headers: Vec::new(),
            body: None,
        }
    }

    fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    #[cfg(test)]
    fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn into_http_request(self) -> (HttpRequest, Option<Vec<u8>>) {
        let mut request = HttpRequest::new(&self.url).with_method(&self.method);
        for (name, value) in &self.headers {
            request = request.with_header(name, value);
        }
        (request, self.body)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaUpdatedPayload<'a> {
    updates: Vec<MediaUpdatedPathPayload<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaUpdatedPathPayload<'a> {
    path: &'a str,
    update_type: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ItemMetadataPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    overview: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    premiere_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_time_ticks: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    studios: Vec<StudioPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sort_name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct StudioPayload {
    name: String,
}

fn iso_datetime(date: &str) -> String {
    let date = date.trim();
    // Date-only values are what Scryer stores; Jellyfin's PremiereDate is a
    // timestamp, so promote them to midnight UTC.
    if date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' {
        format!("{date}T00:00:00Z")
    } else {
        date.to_string()
    }
}

fn default_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "jellyfin".to_string(),
        name: "Jellyfin / Silo".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: "jellyfin".to_string(),
            provider_aliases: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                supports_rich_text: false,
                supports_images: false,
                supports_test: true,
                supports_batch: true,
                supports_coalescing: true,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![NotificationDeliveryMode::MediaServerUpdate],
                payload_formats: vec![NotificationPayloadFormat::StructuredJson],
                supported_events: media_refresh_events(),
                event_options: Default::default(),
            },
            config_fields: vec![
                ConfigFieldDef {
                    key: "base_url".to_string(),
                    label: "Base URL".to_string(),
                    field_type: ConfigFieldType::String,
                    required: true,
                    default_value: None,
                    value_source: Default::default(),
                    host_binding: None,
                    role: None,
                    options: vec![],
                    help_text: Some(
                        "Jellyfin or Silo server URL, for example http://jellyfin:8096".to_string(),
                    ),
                    ..Default::default()
                },
                ConfigFieldDef {
                    key: "api_key".to_string(),
                    label: "API Key".to_string(),
                    field_type: ConfigFieldType::Password,
                    required: true,
                    default_value: None,
                    value_source: Default::default(),
                    host_binding: None,
                    role: None,
                    options: vec![],
                    help_text: Some(
                        "Jellyfin or Silo API key used for targeted refresh calls.".to_string(),
                    ),
                    ..Default::default()
                },
                ConfigFieldDef {
                    key: "path_mappings".to_string(),
                    label: "Path Mappings".to_string(),
                    field_type: ConfigFieldType::Multiline,
                    required: false,
                    default_value: None,
                    value_source: Default::default(),
                    host_binding: None,
                    role: None,
                    options: vec![],
                    help_text: Some(
                        "Pick the Scryer path on the left and enter the matching path as Jellyfin or Silo sees it on the right. Add up to 10 mappings. Most specific path wins. Silo refreshes mapped paths only, so add a mapping for each library even where both sides are the same; the provider-id fallback for unmapped paths works on Jellyfin only."
                            .to_string(),
                    ),
                    ..Default::default()
                },
                ConfigFieldDef {
                    key: "export_images_metadata".to_string(),
                    label: "Export Artwork and Metadata".to_string(),
                    field_type: ConfigFieldType::Bool,
                    required: false,
                    default_value: Some("false".to_string()),
                    value_source: Default::default(),
                    host_binding: None,
                    role: None,
                    options: vec![],
                    help_text: Some(
                        "After a refresh, push Scryer's poster/background images and title metadata onto the Jellyfin item."
                            .to_string(),
                    ),
                    ..Default::default()
                },
            ],
        }),
        settings: Vec::new(),
    }
}

fn media_refresh_events() -> Vec<SdkNotificationEventType> {
    vec![
        SdkNotificationEventType::ImportComplete,
        SdkNotificationEventType::Upgrade,
        SdkNotificationEventType::Rename,
        SdkNotificationEventType::FileDeleted,
        SdkNotificationEventType::FileDeletedForUpgrade,
        SdkNotificationEventType::TitleMoved,
    ]
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
            PluginNotificationCommandResult::Action(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "jellyfin does not implement notification actions",
                None,
            )))
        }
    }
}

fn send_notification(
    request: &PluginNotificationRequest,
) -> PluginResult<PluginNotificationResponse> {
    let config = match JellyfinConfig::from_host() {
        Ok(config) => config,
        Err(error) => {
            return PluginResult::Err(plugin_error(PluginErrorCode::InvalidConfig, error, None));
        }
    };

    let plans = match build_request_plans(request, &config) {
        Ok(plans) => plans,
        Err(error) => return PluginResult::Err(error),
    };

    // The calls of one send run in order and the first failure ends the send:
    // there is no per-target merge, so a failure is the send's typed error.
    for plan in &plans {
        if let Err(error) = execute_plan(plan, &config) {
            return PluginResult::Err(error);
        }
    }

    // Artwork/metadata export is best-effort and opt-in: the refresh above is
    // the notification's contract, and a Jellyfin item may not be indexed yet
    // on the first import. Never fail a delivery because enrichment had no
    // matching item or the image host was unavailable.
    if config.export_images_metadata
        && matches!(
            request.event_type,
            SdkNotificationEventType::ImportComplete | SdkNotificationEventType::Upgrade
        )
    {
        let _ = export_images_and_metadata(request, &config);
    }

    PluginResult::Ok(success_response())
}

/// Preferred provider ids for finding the Jellyfin item. Jellyfin's
/// `AnyProviderIdEquals` uses lowercase provider names and a dot separator.
fn first_provider_id(title: Option<&PluginNotificationTitle>) -> Option<(&'static str, String)> {
    let title = title?;
    for (provider, id) in [
        ("imdb", title.external_ids.imdb_id.as_deref()),
        ("tmdb", title.external_ids.tmdb_id.as_deref()),
        ("tvdb", title.external_ids.tvdb_id.as_deref()),
    ] {
        if let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) {
            return Some((provider, id.to_string()));
        }
    }
    None
}

fn export_images_and_metadata(
    request: &PluginNotificationRequest,
    config: &JellyfinConfig,
) -> Result<(), PluginError> {
    let (provider, id) = first_provider_id(request.title.as_ref()).ok_or_else(|| {
        payload_error("artwork export needs an imdb/tmdb/tvdb id on the title".to_string())
    })?;
    let item_id = resolve_jellyfin_item(provider, &id, config)?.ok_or_else(|| {
        payload_error("no Jellyfin item matched the title provider ids".to_string())
    })?;

    let mut plans = Vec::new();
    if let Some(url) = request
        .title
        .as_ref()
        .and_then(|title| title.poster_url.as_deref())
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        plans.push(JellyfinRequestPlan::DownloadRemoteImage {
            item_id: item_id.clone(),
            image_type: "Primary",
            image_url: url.to_string(),
        });
    }
    if let Some(url) = request
        .title
        .as_ref()
        .and_then(|title| title.background_url.as_deref())
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        plans.push(JellyfinRequestPlan::DownloadRemoteImage {
            item_id: item_id.clone(),
            image_type: "Backdrop",
            image_url: url.to_string(),
        });
    }
    if let Some(fields) = ItemMetadataFields::from_title(request.title.as_ref()) {
        plans.push(JellyfinRequestPlan::UpdateItemMetadata { item_id, fields });
    }

    for plan in plans {
        execute_plan(&plan, config)?;
    }
    Ok(())
}

/// Parse the first item id from Jellyfin's `GET /Items` response.
fn parse_item_id(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("Items")?
        .as_array()?
        .first()?
        .get("Id")?
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn resolve_jellyfin_item(
    provider: &str,
    id: &str,
    config: &JellyfinConfig,
) -> Result<Option<String>, PluginError> {
    let plan = JellyfinRequestPlan::ItemLookup {
        provider: provider.to_string(),
        id: id.to_string(),
    };
    let request = build_http_request(&plan, config)?;
    let (request, body) = request.into_http_request();
    let response = http::request::<Vec<u8>>(&request, body).map_err(|error| {
        plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            "could not look up the Jellyfin item",
            Some(error.to_string()),
        )
    })?;
    if !(200..300).contains(&response.status_code()) {
        return Err(classify_failure(
            response.status_code(),
            retry_after_header(response.headers()),
            &response.body(),
        ));
    }
    Ok(parse_item_id(&response.body()))
}

fn plugin_error(
    code: impl Into<PluginErrorCode>,
    public_message: impl Into<String>,
    debug_message: Option<String>,
) -> PluginError {
    let builder = scryer_plugin_pdk::plugin_error(code, public_message);
    match debug_message {
        Some(debug) => builder.debug(redact::redact_secrets(&debug)).build(),
        None => builder.build(),
    }
}

/// A notification this plugin cannot turn into a Jellyfin call. Resending the
/// same notification fails the same way.
fn payload_error(message: impl Into<String>) -> PluginError {
    plugin_error(PluginErrorCode::Permanent, message, None)
}

impl JellyfinConfig {
    fn from_host() -> Result<Self, String> {
        let base_url = config::get("base_url")
            .ok()
            .flatten()
            .and_then(|value| normalize_base_url(&value))
            .ok_or_else(|| "base_url is not configured".to_string())?;
        let api_key = config::get("api_key")
            .ok()
            .flatten()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "api_key is not configured".to_string())?;
        let path_mappings = config::get("path_mappings")
            .ok()
            .flatten()
            .unwrap_or_default();
        let export_images_metadata = config::get("export_images_metadata")
            .ok()
            .flatten()
            .is_some_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                )
            });

        Ok(Self {
            base_url,
            api_key,
            path_mappings,
            export_images_metadata,
        })
    }
}

fn normalize_base_url(value: &str) -> Option<String> {
    let normalized = value.trim().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn build_request_plans(
    request: &PluginNotificationRequest,
    config: &JellyfinConfig,
) -> Result<Vec<JellyfinRequestPlan>, PluginError> {
    if matches!(request.event_type, SdkNotificationEventType::Test) {
        return Ok(vec![JellyfinRequestPlan::SystemInfo]);
    }
    // A move reports each moved file as a deleted old path and a created new
    // one. A move that relocated no media files carries no file block, and
    // there is then nothing for Jellyfin to rescan.
    if matches!(request.event_type, SdkNotificationEventType::TitleMoved)
        && request
            .file
            .as_ref()
            .is_none_or(|file| file.media_updates.is_empty())
    {
        return Ok(Vec::new());
    }

    let mappings = parse_path_mappings(&config.path_mappings).map_err(|error| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("path_mappings: {error}"),
            None,
        )
    })?;
    let updates = parse_media_updates(request.file.as_ref()).map_err(payload_error)?;
    let external_ids = parse_external_ids(request.title.as_ref());
    let title_facet = request
        .title
        .as_ref()
        .map(|title| title.facet.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut mapped_updates = Vec::new();
    let mut saw_unmapped_update = false;

    for update in updates {
        if let Some(mapped_path) = map_path(&mappings, &update.path) {
            mapped_updates.push(MediaUpdate {
                path: mapped_path,
                update_type: update.update_type,
            });
        } else {
            saw_unmapped_update = true;
        }
    }

    dedupe_updates(&mut mapped_updates);

    let mut plans = Vec::new();
    if !mapped_updates.is_empty() {
        plans.push(JellyfinRequestPlan::MediaUpdated {
            updates: mapped_updates,
        });
    }

    if saw_unmapped_update {
        match title_facet
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "movie" => {
                if external_ids.tmdb_id.is_some() || external_ids.imdb_id.is_some() {
                    plans.push(JellyfinRequestPlan::MoviesUpdated {
                        tmdb_id: external_ids.tmdb_id,
                        imdb_id: external_ids.imdb_id,
                    });
                } else {
                    return Err(payload_error(
                        "found unmapped media updates but no tmdb_id/imdb_id fallback for movie"
                            .to_string(),
                    ));
                }
            }
            "series" | "anime" | "tv" => {
                if let Some(tvdb_id) = external_ids.tvdb_id {
                    plans.push(JellyfinRequestPlan::SeriesUpdated { tvdb_id });
                } else {
                    return Err(payload_error(
                        "found unmapped media updates but no tvdb_id fallback for series/anime"
                            .to_string(),
                    ));
                }
            }
            other => {
                return Err(payload_error(format!(
                    "found unmapped media updates but unsupported or missing title_facet: {other}"
                )));
            }
        }
    }

    if plans.is_empty() {
        return Err(payload_error(
            "notification did not contain any media updates to send".to_string(),
        ));
    }

    Ok(plans)
}

fn parse_media_updates(file: Option<&PluginNotificationFile>) -> Result<Vec<MediaUpdate>, String> {
    let Some(file) = file else {
        return Err("file.media_updates is required".to_string());
    };

    let mut parsed = Vec::with_capacity(file.media_updates.len());
    for item in &file.media_updates {
        let path = item.path.trim();
        if path.is_empty() {
            return Err("file.media_updates[].path is required".to_string());
        }
        let update_type = match item.update_type {
            scryer_plugin_sdk::NotificationMediaUpdateType::Created => MediaUpdateType::Created,
            scryer_plugin_sdk::NotificationMediaUpdateType::Modified => MediaUpdateType::Modified,
            scryer_plugin_sdk::NotificationMediaUpdateType::Deleted => MediaUpdateType::Deleted,
        };
        parsed.push(MediaUpdate {
            path: path.to_string(),
            update_type,
        });
    }

    Ok(parsed)
}

fn parse_external_ids(title: Option<&PluginNotificationTitle>) -> ExternalIds {
    let Some(title) = title else {
        return ExternalIds::default();
    };
    ExternalIds {
        tmdb_id: title
            .external_ids
            .tmdb_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        imdb_id: title
            .external_ids
            .imdb_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        tvdb_id: title
            .external_ids
            .tvdb_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    }
}

fn parse_path_mappings(input: &str) -> Result<Vec<PathMapping>, String> {
    let mut mappings = Vec::new();

    for (index, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((source_prefix, destination_prefix)) = line.split_once("=>") else {
            return Err(format!(
                "invalid path mapping on line {}: expected SOURCE => DESTINATION",
                index + 1
            ));
        };
        let source_prefix = source_prefix.trim();
        let destination_prefix = destination_prefix.trim();
        if !is_absolute_path(source_prefix) || !is_absolute_path(destination_prefix) {
            return Err(format!(
                "invalid path mapping on line {}: both sides must be absolute paths",
                index + 1
            ));
        }

        mappings.push(PathMapping {
            source_prefix: trim_trailing_separator(source_prefix),
            source_prefix_normalized: trim_trailing_separator(&normalize_separators(source_prefix)),
            destination_prefix: trim_trailing_separator(destination_prefix),
        });
    }

    mappings.sort_by(|left, right| {
        right
            .source_prefix_normalized
            .len()
            .cmp(&left.source_prefix_normalized.len())
    });

    Ok(mappings)
}

fn map_path(mappings: &[PathMapping], source_path: &str) -> Option<String> {
    let normalized_path = trim_trailing_separator(&normalize_separators(source_path));

    for mapping in mappings {
        if !prefix_matches(&mapping.source_prefix_normalized, &normalized_path) {
            continue;
        }

        let suffix = &normalized_path[mapping.source_prefix_normalized.len()..];
        let preferred_separator = if mapping.destination_prefix.contains('\\')
            && !mapping.destination_prefix.contains('/')
        {
            '\\'
        } else {
            '/'
        };
        let mut converted_suffix = suffix.replace('/', &preferred_separator.to_string());
        if !converted_suffix.is_empty() && !converted_suffix.starts_with(preferred_separator) {
            converted_suffix.insert(0, preferred_separator);
        }
        return Some(format!(
            "{}{}",
            mapping.destination_prefix, converted_suffix
        ));
    }

    None
}

fn dedupe_updates(updates: &mut Vec<MediaUpdate>) {
    let mut seen = HashSet::new();
    updates.retain(|update| seen.insert((update.path.clone(), update.update_type)));
}

fn prefix_matches(prefix: &str, full_path: &str) -> bool {
    full_path == prefix
        || (full_path.starts_with(prefix)
            && full_path
                .as_bytes()
                .get(prefix.len())
                .is_some_and(|byte| *byte == b'/'))
}

fn is_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with("\\\\")
        || (bytes.len() >= 3
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
            && bytes[0].is_ascii_alphabetic())
}

fn normalize_separators(value: &str) -> String {
    value.replace('\\', "/")
}

fn trim_trailing_separator(value: &str) -> String {
    if value == "/" {
        return value.to_string();
    }
    value.trim_end_matches(['/', '\\']).to_string()
}

fn execute_plan(plan: &JellyfinRequestPlan, config: &JellyfinConfig) -> Result<(), PluginError> {
    let request = build_http_request(plan, config)?;
    execute_http(request, plan)
}

fn build_http_request(
    plan: &JellyfinRequestPlan,
    config: &JellyfinConfig,
) -> Result<PreparedHttpRequest, PluginError> {
    match plan {
        JellyfinRequestPlan::SystemInfo => Ok(base_request(
            "GET",
            &format!("{}/System/Info", config.base_url),
            config,
        )),
        JellyfinRequestPlan::MediaUpdated { updates } => {
            let payload = MediaUpdatedPayload {
                updates: updates
                    .iter()
                    .map(|update| MediaUpdatedPathPayload {
                        path: update.path.as_str(),
                        update_type: update.update_type.as_jellyfin(),
                    })
                    .collect(),
            };
            let body = serde_json::to_vec(&payload).map_err(|error| {
                plugin_error(
                    PluginErrorCode::Permanent,
                    "the Jellyfin media update payload could not be encoded",
                    Some(error.to_string()),
                )
            })?;
            Ok(base_request(
                "POST",
                &format!("{}/Library/Media/Updated", config.base_url),
                config,
            )
            .with_header("Content-Type", "application/json")
            .with_body(body))
        }
        JellyfinRequestPlan::MoviesUpdated { tmdb_id, imdb_id } => {
            let mut query = Vec::new();
            if let Some(tmdb_id) = tmdb_id {
                query.push(format!("tmdbId={}", encode_query_value(tmdb_id)));
            }
            if let Some(imdb_id) = imdb_id {
                query.push(format!("imdbId={}", encode_query_value(imdb_id)));
            }
            let url = if query.is_empty() {
                format!("{}/Library/Movies/Updated", config.base_url)
            } else {
                format!(
                    "{}/Library/Movies/Updated?{}",
                    config.base_url,
                    query.join("&")
                )
            };
            Ok(base_request("POST", &url, config))
        }
        JellyfinRequestPlan::SeriesUpdated { tvdb_id } => Ok(base_request(
            "POST",
            &format!(
                "{}/Library/Series/Updated?tvdbId={}",
                config.base_url,
                encode_query_value(tvdb_id)
            ),
            config,
        )),
        JellyfinRequestPlan::ItemLookup { provider, id } => Ok(base_request(
            "GET",
            &format!(
                "{}/Items?AnyProviderIdEquals={}.{}&Fields=Id&Limit=1",
                config.base_url,
                encode_query_value(provider),
                encode_query_value(id)
            ),
            config,
        )),
        JellyfinRequestPlan::DownloadRemoteImage {
            item_id,
            image_type,
            image_url,
        } => Ok(base_request(
            "POST",
            &format!(
                "{}/Items/{}/RemoteImages/Download?type={}&imageUrl={}&providerName=Scryer",
                config.base_url,
                encode_query_value(item_id),
                encode_query_value(image_type),
                encode_query_value(image_url)
            ),
            config,
        )),
        JellyfinRequestPlan::UpdateItemMetadata { item_id, fields } => {
            let payload = ItemMetadataPayload {
                overview: fields.overview.clone(),
                tags: fields.tags.clone(),
                premiere_date: fields.release_date.as_deref().map(iso_datetime),
                run_time_ticks: fields
                    .runtime_minutes
                    .map(|minutes| i64::from(minutes) * 60 * 10_000_000),
                studios: fields
                    .studio
                    .iter()
                    .map(|name| StudioPayload { name: name.clone() })
                    .collect(),
                sort_name: fields.sort_title.clone(),
            };
            let body = serde_json::to_vec(&payload).map_err(|error| {
                plugin_error(
                    PluginErrorCode::Permanent,
                    "the Jellyfin item metadata payload could not be encoded",
                    Some(error.to_string()),
                )
            })?;
            Ok(base_request(
                "POST",
                &format!("{}/Items/{}", config.base_url, encode_query_value(item_id)),
                config,
            )
            .with_header("Content-Type", "application/json")
            .with_body(body))
        }
    }
}

fn base_request(method: &str, url: &str, config: &JellyfinConfig) -> PreparedHttpRequest {
    PreparedHttpRequest::new(method, url.to_string())
        .with_header(
            "Authorization",
            format!("MediaBrowser Token=\"{}\"", config.api_key),
        )
        .with_header("Accept", "application/json")
        .with_header("User-Agent", "scryer-jellyfin-plugin/0.1")
}

fn execute_http(
    prepared: PreparedHttpRequest,
    plan: &JellyfinRequestPlan,
) -> Result<(), PluginError> {
    let (request, body) = prepared.into_http_request();
    match http::request::<Vec<u8>>(&request, body) {
        Ok(response) => {
            let status = response.status_code();
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(classify_failure(
                    plan,
                    status,
                    retry_after_header(response.headers()),
                    &response.body(),
                ))
            }
        }
        Err(error) => {
            let mut error = scryer_plugin_pdk::error::plugin_error_from(&error);
            error.debug_message = error
                .debug_message
                .map(|debug| redact::redact_secrets(&debug));
            Err(error)
        }
    }
}

/// A non-success Jellyfin answer. Jellyfin checks only the API key, so a 401
/// or 403 is a rejected key. A 404 on a provider-id refresh means the server
/// has no such endpoint (Silo), not a wrong base URL. The response body goes
/// to the debug log only.
fn classify_failure(
    plan: &JellyfinRequestPlan,
    status: u16,
    retry_after: Option<i64>,
    body: &[u8],
) -> PluginError {
    let debug = format!("HTTP {status}: {}", String::from_utf8_lossy(body));
    let builder = match status {
        404 if plan.is_provider_id_refresh() => scryer_plugin_pdk::plugin_error(
            PluginErrorCode::Unsupported,
            "the server does not support refresh by provider id; add a path mapping that covers this title's library",
        ),
        401 | 403 => scryer_plugin_pdk::plugin_error(
            codes::INVALID_API_KEY,
            "Jellyfin rejected the configured api_key",
        ),
        404 => scryer_plugin_pdk::plugin_error(
            codes::ENDPOINT_NOT_FOUND,
            "base_url does not answer like a Jellyfin server",
        ),
        429 => {
            let builder = scryer_plugin_pdk::plugin_error(
                PluginErrorCode::RateLimited,
                "Jellyfin is rate limiting requests",
            );
            match retry_after {
                Some(seconds) => builder.retry_after_seconds(seconds),
                None => builder,
            }
        }
        500..=599 => scryer_plugin_pdk::plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            "Jellyfin answered with a server error",
        ),
        _ => scryer_plugin_pdk::plugin_error(
            PluginErrorCode::Permanent,
            "Jellyfin refused the library refresh",
        ),
    };
    builder.debug(redact::redact_secrets(&debug)).build()
}

/// A `Retry-After` header in delta-seconds, if the response carried one.
fn retry_after_header(headers: &std::collections::BTreeMap<String, String>) -> Option<i64> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| (seconds.ceil() as i64).max(1))
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn success_response() -> PluginNotificationResponse {
    PluginNotificationResponse {
        success: true,
        error: None,
        delivery_id: None,
        provider_status: None,
        retry_after_seconds: None,
        warnings: Vec::new(),
        target_results: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_metadata(
        event_type: &str,
        title_facet: &str,
        metadata: serde_json::Value,
    ) -> PluginNotificationRequest {
        let metadata = metadata.as_object().unwrap();
        let external_ids = metadata
            .get("external_ids")
            .and_then(serde_json::Value::as_object);
        let media_updates: Vec<scryer_plugin_sdk::PluginNotificationMediaUpdate> = metadata
            .get("media_updates")
            .and_then(serde_json::Value::as_array)
            .map(|updates| {
                updates
                    .iter()
                    .map(|update| scryer_plugin_sdk::PluginNotificationMediaUpdate {
                        path: update["path"].as_str().unwrap_or_default().to_string(),
                        update_type: match update["update_type"].as_str().unwrap_or_default() {
                            "created" => scryer_plugin_sdk::NotificationMediaUpdateType::Created,
                            "modified" => scryer_plugin_sdk::NotificationMediaUpdateType::Modified,
                            "deleted" => scryer_plugin_sdk::NotificationMediaUpdateType::Deleted,
                            other => panic!("unsupported update_type in test fixture: {other}"),
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        PluginNotificationRequest {
            schema_version: 1,
            event_type: match event_type {
                "import_complete" => SdkNotificationEventType::ImportComplete,
                "upgrade" => SdkNotificationEventType::Upgrade,
                "rename" => SdkNotificationEventType::Rename,
                "file_deleted" => SdkNotificationEventType::FileDeleted,
                "title_moved" => SdkNotificationEventType::TitleMoved,
                "test" => SdkNotificationEventType::Test,
                other => panic!("unsupported event_type in test fixture: {other}"),
            },
            event_id: Some("evt-1".to_string()),
            occurred_at: Some("2026-04-29T12:00:00Z".to_string()),
            correlation_id: Some("corr-1".to_string()),
            actor: None,
            severity: None,
            is_test: event_type == "test",
            summary_title: "Test".to_string(),
            summary_message: "Body".to_string(),
            app: scryer_plugin_sdk::PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "test".to_string(),
            },
            title: Some(PluginNotificationTitle {
                id: None,
                name: "Test".to_string(),
                facet: title_facet.to_string(),
                year: None,
                slug: None,
                path: None,
                overview: None,
                sort_title: None,
                background_url: None,
                poster_url: None,
                tags: Vec::new(),
                aliases: Vec::new(),
                original_language: None,
                original_country: None,
                release_date: metadata
                    .get("release_date")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                runtime_minutes: metadata
                    .get("runtime_minutes")
                    .and_then(serde_json::Value::as_i64)
                    .map(|value| value as i32),
                studio: metadata
                    .get("studio")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                network: metadata
                    .get("network")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                content_status: metadata
                    .get("content_status")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                external_ids: scryer_plugin_sdk::PluginNotificationExternalIds {
                    tmdb_id: external_ids
                        .and_then(|ids| ids.get("tmdb_id"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    imdb_id: external_ids
                        .and_then(|ids| ids.get("imdb_id"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    tvdb_id: external_ids
                        .and_then(|ids| ids.get("tvdb_id"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    anidb_id: None,
                    tvmaze_id: None,
                    anilist_ids: Vec::new(),
                    mal_ids: Vec::new(),
                    kitsu_ids: Vec::new(),
                    by_source: Default::default(),
                    by_source_key: Default::default(),
                },
            }),
            episode: None,
            episodes: Vec::new(),
            release: None,
            download: None,
            import: None,
            health: None,
            file: Some(PluginNotificationFile {
                primary_path: media_updates.first().map(|update| update.path.clone()),
                media_updates,
            }),
            media_files: Vec::new(),
            application_update: None,
            manual_interaction: None,
            media_request: None,
            title_move: None,
        }
    }

    fn config(path_mappings: &str) -> JellyfinConfig {
        JellyfinConfig {
            base_url: "http://jellyfin:8096".to_string(),
            api_key: "secret".to_string(),
            path_mappings: path_mappings.to_string(),
            export_images_metadata: false,
        }
    }

    #[test]
    fn describe_includes_expected_fields() {
        let descriptor = default_descriptor();
        let json = serde_json::to_value(descriptor).unwrap();
        assert_eq!(json["provider"]["provider_type"], "jellyfin");
        assert_eq!(json["provider"]["kind"], "notification");
        assert_eq!(
            json["provider"]["config_fields"][2]["field_type"],
            "multiline"
        );
    }

    #[test]
    fn descriptor_supports_only_media_refresh_events() {
        let notification = match default_descriptor().provider {
            ProviderDescriptor::Notification(notification) => notification,
            provider => panic!("expected notification provider, got {provider:?}"),
        };
        let events = notification.capabilities.supported_events;
        assert!(events.contains(&SdkNotificationEventType::ImportComplete));
        assert!(events.contains(&SdkNotificationEventType::Upgrade));
        assert!(events.contains(&SdkNotificationEventType::Rename));
        assert!(events.contains(&SdkNotificationEventType::FileDeleted));
        assert!(events.contains(&SdkNotificationEventType::FileDeletedForUpgrade));
        assert!(!events.contains(&SdkNotificationEventType::MediaRequestSubmitted));
        assert!(!events.contains(&SdkNotificationEventType::MediaRequestApproved));
        assert!(!events.contains(&SdkNotificationEventType::MediaRequestRejected));
        assert!(!events.contains(&SdkNotificationEventType::MediaRequestCanceled));
    }

    #[test]
    fn normalize_base_url_trims_whitespace_and_trailing_slashes() {
        assert_eq!(
            normalize_base_url("  http://jellyfin:8096/// "),
            Some("http://jellyfin:8096".to_string())
        );
        assert_eq!(normalize_base_url("   "), None);
    }

    #[test]
    fn parse_path_mappings_ignores_comments_and_blanks() {
        let mappings = parse_path_mappings(
            r#"
                # comment
                /data/media => /mnt/media

                /data/media/anime => /srv/anime
            "#,
        )
        .unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].source_prefix, "/data/media/anime");
    }

    #[test]
    fn map_path_uses_longest_prefix_and_boundary_safe_matching() {
        let mappings = parse_path_mappings(
            "/data/media => /mnt/media\n/data/media/anime => /srv/anime\n/data/tv => /mnt/tv",
        )
        .unwrap();
        assert_eq!(
            map_path(&mappings, "/data/media/anime/Show/E01.mkv").unwrap(),
            "/srv/anime/Show/E01.mkv"
        );
        assert_eq!(map_path(&mappings, "/data/media2/Show.mkv"), None);
    }

    #[test]
    fn parse_path_mappings_supports_windows_and_unc_paths() {
        let mappings = parse_path_mappings(
            r#"
                C:\Media\TV => D:\Jellyfin\TV
                \\nas\anime => /mnt/anime
            "#,
        )
        .unwrap();

        assert_eq!(mappings.len(), 2);
        assert_eq!(
            map_path(&mappings, r"C:\Media\TV\Show\S01E01.mkv").as_deref(),
            Some(r"D:\Jellyfin\TV\Show\S01E01.mkv")
        );
        assert_eq!(
            map_path(&mappings, r"\\nas\anime\Show\E01.mkv").as_deref(),
            Some("/mnt/anime/Show/E01.mkv")
        );
    }

    #[test]
    fn build_request_plans_prefers_mapped_media_updates() {
        let request = request_with_metadata(
            "import_complete",
            "series",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/tv/Show/S01E01.mkv", "update_type": "created" }
                ],
                "external_ids": { "tvdb_id": "12345" }
            }),
        );

        let plans = build_request_plans(&request, &config("/data/tv => /mnt/tv")).unwrap();
        assert_eq!(
            plans,
            vec![JellyfinRequestPlan::MediaUpdated {
                updates: vec![MediaUpdate {
                    path: "/mnt/tv/Show/S01E01.mkv".to_string(),
                    update_type: MediaUpdateType::Created,
                }],
            }]
        );
    }

    #[test]
    fn build_request_plans_for_a_title_move_rescan_both_paths() {
        assert!(media_refresh_events().contains(&SdkNotificationEventType::TitleMoved));
        let request = request_with_metadata(
            "title_moved",
            "series",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/tv/Old Root/Show/S01E01.mkv", "update_type": "deleted" },
                    { "path": "/data/tv/New Root/Show/S01E01.mkv", "update_type": "created" }
                ],
                "external_ids": { "tvdb_id": "12345" }
            }),
        );

        let plans = build_request_plans(&request, &config("/data/tv => /mnt/tv")).unwrap();
        assert_eq!(
            plans,
            vec![JellyfinRequestPlan::MediaUpdated {
                updates: vec![
                    MediaUpdate {
                        path: "/mnt/tv/Old Root/Show/S01E01.mkv".to_string(),
                        update_type: MediaUpdateType::Deleted,
                    },
                    MediaUpdate {
                        path: "/mnt/tv/New Root/Show/S01E01.mkv".to_string(),
                        update_type: MediaUpdateType::Created,
                    },
                ],
            }]
        );
    }

    #[test]
    fn build_request_plans_for_a_title_move_without_files_sends_nothing() {
        let mut request = request_with_metadata(
            "title_moved",
            "series",
            serde_json::json!({ "external_ids": { "tvdb_id": "12345" } }),
        );
        request.file = None;
        assert_eq!(
            build_request_plans(&request, &config("/data/tv => /mnt/tv")).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn build_request_plans_combines_mapped_updates_with_movie_id_fallbacks() {
        let request = request_with_metadata(
            "upgrade",
            "movie",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/movies/Movie (2024)/Movie.mkv", "update_type": "modified" },
                    { "path": "/data/other/Movie (2024)/Movie Extras.mkv", "update_type": "created" }
                ],
                "external_ids": { "tmdb_id": "987", "imdb_id": "tt1234567" }
            }),
        );

        let plans = build_request_plans(&request, &config("/data/movies => /mnt/movies")).unwrap();
        assert_eq!(
            plans,
            vec![
                JellyfinRequestPlan::MediaUpdated {
                    updates: vec![MediaUpdate {
                        path: "/mnt/movies/Movie (2024)/Movie.mkv".to_string(),
                        update_type: MediaUpdateType::Modified,
                    }],
                },
                JellyfinRequestPlan::MoviesUpdated {
                    tmdb_id: Some("987".to_string()),
                    imdb_id: Some("tt1234567".to_string()),
                },
            ]
        );
    }

    #[test]
    fn build_request_plans_trims_external_ids_before_series_fallback() {
        let request = request_with_metadata(
            "rename",
            "series",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/tv/Show/S01E01.mkv", "update_type": "modified" }
                ],
                "external_ids": { "tvdb_id": " 4242 " }
            }),
        );

        let plans = build_request_plans(&request, &config("")).unwrap();
        assert_eq!(
            plans,
            vec![JellyfinRequestPlan::SeriesUpdated {
                tvdb_id: "4242".to_string(),
            }]
        );
    }

    #[test]
    fn build_request_plans_falls_back_to_movie_ids_for_unmapped_updates() {
        let request = request_with_metadata(
            "upgrade",
            "movie",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/movies/Movie (2024)/Movie.mkv", "update_type": "modified" }
                ],
                "external_ids": { "tmdb_id": "987", "imdb_id": "tt1234567" }
            }),
        );

        let plans = build_request_plans(&request, &config("/data/tv => /mnt/tv")).unwrap();
        assert_eq!(
            plans,
            vec![JellyfinRequestPlan::MoviesUpdated {
                tmdb_id: Some("987".to_string()),
                imdb_id: Some("tt1234567".to_string()),
            }]
        );
    }

    #[test]
    fn build_request_plans_falls_back_to_series_ids_for_unmapped_updates() {
        let request = request_with_metadata(
            "rename",
            "anime",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/anime/Show/S01E01.mkv", "update_type": "deleted" }
                ],
                "external_ids": { "tvdb_id": "4242" }
            }),
        );

        let plans = build_request_plans(&request, &config("")).unwrap();
        assert_eq!(
            plans,
            vec![JellyfinRequestPlan::SeriesUpdated {
                tvdb_id: "4242".to_string(),
            }]
        );
    }

    #[test]
    fn build_request_plans_errors_when_unmapped_updates_have_no_fallback_ids() {
        let request = request_with_metadata(
            "file_deleted",
            "movie",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/movies/Movie (2024)/Movie.mkv", "update_type": "deleted" }
                ]
            }),
        );

        let error = build_request_plans(&request, &config("")).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("tmdb_id/imdb_id"));
    }

    #[test]
    fn build_request_plans_dedupes_identical_mapped_updates() {
        let request = request_with_metadata(
            "rename",
            "series",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/tv/Show/S01E01.mkv", "update_type": "created" },
                    { "path": "/data/tv/Show/S01E01.mkv", "update_type": "created" }
                ],
                "external_ids": { "tvdb_id": "12345" }
            }),
        );

        let plans = build_request_plans(&request, &config("/data/tv => /mnt/tv")).unwrap();
        match &plans[0] {
            JellyfinRequestPlan::MediaUpdated { updates } => {
                assert_eq!(updates.len(), 1);
                assert_eq!(updates[0].path, "/mnt/tv/Show/S01E01.mkv");
            }
            _ => panic!("expected media updated plan"),
        }
    }

    #[test]
    fn build_request_plans_for_test_event_only_checks_system_info() {
        let request = request_with_metadata("test", "movie", serde_json::json!({}));
        let plans = build_request_plans(&request, &config("")).unwrap();
        assert_eq!(plans, vec![JellyfinRequestPlan::SystemInfo]);
    }

    #[test]
    fn build_http_request_for_system_info_uses_get_and_auth_headers() {
        let request = build_http_request(
            &JellyfinRequestPlan::SystemInfo,
            &JellyfinConfig {
                base_url: "http://jellyfin:8096/".trim_end_matches('/').to_string(),
                api_key: "secret".to_string(),
                path_mappings: String::new(),
                export_images_metadata: false,
            },
        )
        .unwrap();

        assert_eq!(request.method, "GET");
        assert_eq!(request.url, "http://jellyfin:8096/System/Info");
        assert_eq!(
            request.header_value("Authorization"),
            Some("MediaBrowser Token=\"secret\"")
        );
        assert_eq!(request.header_value("Accept"), Some("application/json"));
        assert_eq!(request.body, None);
    }

    #[test]
    fn build_http_request_for_media_updated_uses_expected_json_body() {
        let request = build_http_request(
            &JellyfinRequestPlan::MediaUpdated {
                updates: vec![MediaUpdate {
                    path: "/mnt/media/Movies/Movie.mkv".to_string(),
                    update_type: MediaUpdateType::Created,
                }],
            },
            &config(""),
        )
        .unwrap();

        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "http://jellyfin:8096/Library/Media/Updated");
        assert_eq!(
            request.header_value("Content-Type"),
            Some("application/json")
        );

        let body: serde_json::Value =
            serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "updates": [
                    {
                        "path": "/mnt/media/Movies/Movie.mkv",
                        "updateType": "Created"
                    }
                ]
            })
        );
    }

    #[test]
    fn build_http_request_for_movies_updated_encodes_query_values() {
        let request = build_http_request(
            &JellyfinRequestPlan::MoviesUpdated {
                tmdb_id: Some("12 34".to_string()),
                imdb_id: Some("tt/123".to_string()),
            },
            &config(""),
        )
        .unwrap();

        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "http://jellyfin:8096/Library/Movies/Updated?tmdbId=12%2034&imdbId=tt%2F123"
        );
        assert_eq!(request.body, None);
    }

    #[test]
    fn build_http_request_for_series_updated_uses_tvdb_query() {
        let request = build_http_request(
            &JellyfinRequestPlan::SeriesUpdated {
                tvdb_id: "tvdb:123".to_string(),
            },
            &config(""),
        )
        .unwrap();

        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "http://jellyfin:8096/Library/Series/Updated?tvdbId=tvdb%3A123"
        );
        assert_eq!(request.body, None);
    }

    #[test]
    fn a_rejected_api_key_carries_the_invalid_api_key_code() {
        for status in [401, 403] {
            let error = classify_failure(
                &JellyfinRequestPlan::SystemInfo,
                status,
                None,
                b"invalid token for synthetic-user",
            );
            assert_eq!(error.code, PluginErrorCode::from(codes::INVALID_API_KEY));
            assert!(!error.public_message.contains("synthetic-user"));
            assert_eq!(
                error.debug_message.as_deref(),
                Some(format!("HTTP {status}: invalid token for synthetic-user").as_str())
            );
        }
    }

    #[test]
    fn other_jellyfin_answers_map_to_their_codes() {
        assert_eq!(
            classify_failure(&JellyfinRequestPlan::SystemInfo, 404, None, b"").code,
            PluginErrorCode::from(codes::ENDPOINT_NOT_FOUND)
        );
        let limited = classify_failure(&JellyfinRequestPlan::SystemInfo, 429, Some(9), b"");
        assert_eq!(limited.code, PluginErrorCode::RateLimited);
        assert_eq!(limited.retry_after_seconds, Some(9));
        assert_eq!(
            classify_failure(&JellyfinRequestPlan::SystemInfo, 502, None, b"").code,
            PluginErrorCode::UpstreamUnavailable
        );
        assert_eq!(
            classify_failure(&JellyfinRequestPlan::SystemInfo, 400, None, b"").code,
            PluginErrorCode::Permanent
        );
    }

    #[test]
    fn a_missing_provider_id_refresh_endpoint_is_unsupported() {
        let fallbacks = [
            JellyfinRequestPlan::MoviesUpdated {
                tmdb_id: Some("101".to_string()),
                imdb_id: None,
            },
            JellyfinRequestPlan::SeriesUpdated {
                tvdb_id: "202".to_string(),
            },
        ];
        for plan in &fallbacks {
            let error = classify_failure(plan, 404, None, b"404 page not found");
            assert_eq!(error.code, PluginErrorCode::Unsupported);
            assert!(error.public_message.contains("path mapping"));
            assert_eq!(
                error.debug_message.as_deref(),
                Some("HTTP 404: 404 page not found")
            );
        }
    }

    #[test]
    fn a_missing_path_refresh_endpoint_is_still_endpoint_not_found() {
        let media_updated = JellyfinRequestPlan::MediaUpdated {
            updates: vec![MediaUpdate {
                path: "/media/Synthetic Show/S01E01.mkv".to_string(),
                update_type: MediaUpdateType::Created,
            }],
        };
        for plan in [&media_updated, &JellyfinRequestPlan::SystemInfo] {
            assert_eq!(
                classify_failure(plan, 404, None, b"").code,
                PluginErrorCode::from(codes::ENDPOINT_NOT_FOUND)
            );
        }
    }

    #[test]
    fn an_invalid_path_mapping_names_the_setting() {
        let request = request_with_metadata(
            "import_complete",
            "movie",
            serde_json::json!({
                "media_updates": [
                    { "path": "/data/movies/Synthetic Film (2024)/Synthetic.Film.mkv", "update_type": "created" }
                ]
            }),
        );
        let error = build_request_plans(&request, &config("relative => /mnt")).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.starts_with("path_mappings: "));
    }

    #[test]
    fn item_lookup_and_remote_image_requests_use_jellyfin_endpoints() {
        let cfg = config("");
        let lookup = build_http_request(
            &JellyfinRequestPlan::ItemLookup {
                provider: "imdb".to_string(),
                id: "tt0114709".to_string(),
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(lookup.method, "GET");
        assert_eq!(
            lookup.url,
            "http://jellyfin:8096/Items?AnyProviderIdEquals=imdb.tt0114709&Fields=Id&Limit=1"
        );

        let image = build_http_request(
            &JellyfinRequestPlan::DownloadRemoteImage {
                item_id: "item-1".to_string(),
                image_type: "Primary",
                image_url: "https://example.invalid/poster.jpg".to_string(),
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(image.method, "POST");
        assert_eq!(
            image.url,
            "http://jellyfin:8096/Items/item-1/RemoteImages/Download?type=Primary&imageUrl=https%3A%2F%2Fexample.invalid%2Fposter.jpg&providerName=Scryer"
        );
    }

    #[test]
    fn metadata_update_request_carries_title_fields() {
        let fields = ItemMetadataFields {
            overview: Some("Overview".to_string()),
            tags: vec!["tag-1".to_string()],
            release_date: Some("2024-01-02".to_string()),
            runtime_minutes: Some(95),
            studio: Some("Studio".to_string()),
            network: None,
            content_status: None,
            sort_title: Some("Sort Title".to_string()),
        };
        let request = build_http_request(
            &JellyfinRequestPlan::UpdateItemMetadata {
                item_id: "item-1".to_string(),
                fields,
            },
            &config(""),
        )
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "http://jellyfin:8096/Items/item-1");
        let body = request.body.expect("metadata update has a JSON body");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["Overview"], "Overview");
        assert_eq!(json["Tags"], serde_json::json!(["tag-1"]));
        assert_eq!(json["PremiereDate"], "2024-01-02T00:00:00Z");
        assert_eq!(json["RunTimeTicks"], 95_i64 * 60 * 10_000_000);
        assert_eq!(json["Studios"], serde_json::json!([{"Name": "Studio"}]));
        assert_eq!(json["SortName"], "Sort Title");
        assert!(json.get("Network").is_none());
    }

    #[test]
    fn item_id_is_read_from_the_first_lookup_result() {
        assert_eq!(
            parse_item_id(br#"{"Items":[{"Id":"item-1"},{"Id":"item-2"}]}"#).as_deref(),
            Some("item-1")
        );
        assert_eq!(parse_item_id(br#"{"Items":[]}"#), None);
        assert_eq!(parse_item_id(b"not json"), None);
    }

    #[test]
    fn provider_ids_are_preferred_imdb_then_tmdb_then_tvdb() {
        let request = request_with_metadata(
            "import_complete",
            "movie",
            serde_json::json!({
                "external_ids": {"imdb_id": " tt123 ", "tmdb_id": "456", "tvdb_id": "789"}
            }),
        );
        assert_eq!(
            first_provider_id(request.title.as_ref()),
            Some(("imdb", "tt123".to_string()))
        );

        let request = request_with_metadata(
            "import_complete",
            "movie",
            serde_json::json!({"external_ids": {"tmdb_id": "456"}}),
        );
        assert_eq!(
            first_provider_id(request.title.as_ref()),
            Some(("tmdb", "456".to_string()))
        );
    }
}
