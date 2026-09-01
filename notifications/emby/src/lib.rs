//! Emby media-server notifications, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:notification/notification@1.0.0`: two exports
//! carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`, `process`
//! exchanges a `PluginCommandRequest` for a `PluginCommandResponse`), plus the
//! shared `scryer:host/services@1.0.0` import that carries config and HTTP.
//!
//! The refresh planner, the path mapping and the Emby API calls are untouched.
//! What changed is the transport: the two Extism entry points collapse into one
//! `process` export dispatching the SDK's `PluginNotificationCommand`, and
//! `config::get` / `http::request` reach Scryer through `notify-common`'s
//! re-export of [`scryer_plugin_pdk`] rather than the removed core-module host
//! ABI. `result_json` is gone: the world's dispatch owns the `PluginResult`
//! envelope now, so the send path returns the typed response.

use std::cmp::Reverse;
use std::collections::HashSet;

use notify_common::*;

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
    descriptor = default_descriptor,
    handler = handle_notification_command,
);

const PROVIDER_TYPE: &str = "emby";
use scryer_plugin_sdk::{
    NotificationMediaUpdateType, PluginNotificationFile, PluginNotificationTitle,
};

const MAX_PATH_MAPPINGS: usize = 10;
const MAX_JSON_RESPONSE_BYTES: usize = 1024 * 1024;

struct EmbyConfig {
    base_url: String,
    api_key: String,
    path_mappings: Vec<PathMapping>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathMapping {
    source_prefix: String,
    destination_prefix: String,
    destination_separator: char,
    case_insensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MediaUpdateType {
    Created,
    Modified,
    Deleted,
}

impl MediaUpdateType {
    fn as_emby(self) -> &'static str {
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
    tvdb_id: Option<String>,
    imdb_id: Option<String>,
    tmdb_id: Option<String>,
    tvmaze_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbyItemType {
    Series,
    Movie,
}

impl EmbyItemType {
    fn as_emby(self) -> &'static str {
        match self {
            Self::Series => "Series",
            Self::Movie => "Movie",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ItemLookup {
    item_type: EmbyItemType,
    title: String,
    year: Option<i64>,
    external_ids: ExternalIds,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MediaRefreshPlan {
    updates: Vec<MediaUpdate>,
    lookup: Option<ItemLookup>,
    lookup_update_types: Vec<MediaUpdateType>,
}

#[derive(Clone, PartialEq, Eq)]
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

fn default_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "emby",
        "Emby",
        env!("CARGO_PKG_VERSION"),
        "emby",
        vec![NotificationDeliveryMode::MediaServerUpdate],
        vec![NotificationPayloadFormat::StructuredJson],
        vec![
            field(
                "base_url",
                "Base URL",
                ConfigFieldType::String,
                true,
                None,
                Some("Emby server URL, for example http://emby:8096."),
            ),
            field(
                "api_key",
                "API Key",
                ConfigFieldType::Password,
                true,
                None,
                Some("Emby API key used for targeted library refreshes."),
            ),
            field(
                "path_mappings",
                "Path Mappings",
                ConfigFieldType::Multiline,
                false,
                None,
                Some(
                    "One absolute SOURCE => DESTINATION mapping per line. Add up to 10 mappings; the most specific source path wins.",
                ),
            ),
        ],
        false,
        false,
    );

    if let ProviderDescriptor::Notification(notification) = &mut descriptor.provider {
        notification.provider_aliases = vec!["mediabrowser".to_string()];
        notification.capabilities.supports_batch = true;
        notification.capabilities.supports_coalescing = true;
        notification.capabilities.supported_events = media_refresh_events();
    }

    descriptor
}

fn media_refresh_events() -> Vec<NotificationEventType> {
    vec![
        NotificationEventType::ImportComplete,
        NotificationEventType::Upgrade,
        NotificationEventType::Rename,
        NotificationEventType::FileDeleted,
        NotificationEventType::FileDeletedForUpgrade,
    ]
}

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per Extism entry point this plugin used to export. `action` is not
/// one of them: the descriptor advertises no action, so the host does not route
/// one here and the arm answers **in-band** with `Unsupported` rather than
/// trapping. A trap under a component costs the whole instance and replaces the
/// plugin's own diagnosis with a generic ABI failure.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(PluginResult::Ok(send_notification(&request)))
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}

fn send_notification(request: &PluginNotificationRequest) -> PluginNotificationResponse {
    let config = match EmbyConfig::from_host() {
        Ok(config) => config,
        Err(error) => {
            return error_response(error, Some("invalid_config".into()));
        }
    };

    if matches!(request.event_type, NotificationEventType::Test) {
        return match execute_http(build_system_info_request(&config), "Emby server test") {
            Ok(_) => ok_response(),
            Err(error) => error_response(error, None),
        };
    }

    match build_media_refresh_plan(request, &config.path_mappings)
        .and_then(|plan| execute_media_refresh(plan, &config))
    {
        Ok(()) => ok_response(),
        Err(error) => error_response(error, None),
    }
}

impl EmbyConfig {
    fn from_host() -> Result<Self, String> {
        Self::from_lookup(|key| config::get(key).ok().flatten())
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let base_url = lookup("base_url")
            .and_then(nonempty)
            .ok_or_else(|| "emby base_url is not configured".to_string())
            .and_then(|value| normalize_base_url(&value))?;
        let api_key = lookup("api_key")
            .and_then(nonempty)
            .ok_or_else(|| "emby api_key is not configured".to_string())?;

        let path_mappings = lookup("path_mappings")
            .map(|value| parse_path_mappings(&value))
            .transpose()?
            .unwrap_or_default();

        Ok(Self {
            base_url,
            api_key,
            path_mappings,
        })
    }
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn normalize_base_url(value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('/');
    let Some((scheme, remainder)) = value.split_once("://") else {
        return Err("emby base_url must be an absolute http or https URL".to_string());
    };
    let authority = remainder.split('/').next().unwrap_or_default();
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        || remainder.is_empty()
        || remainder.starts_with('/')
        || authority.contains('@')
        || value.contains(char::is_whitespace)
        || value.contains('?')
        || value.contains('#')
    {
        return Err("emby base_url must be an absolute http or https URL".to_string());
    }
    Ok(value.to_string())
}

fn parse_path_mappings(input: &str) -> Result<Vec<PathMapping>, String> {
    let mut mappings = Vec::new();

    for (index, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if mappings.len() == MAX_PATH_MAPPINGS {
            return Err(format!(
                "emby path_mappings supports at most {MAX_PATH_MAPPINGS} mappings"
            ));
        }

        let Some((source, destination)) = line.split_once("=>") else {
            return Err(format!(
                "invalid emby path mapping on line {}: expected SOURCE => DESTINATION",
                index + 1
            ));
        };
        if destination.contains("=>") {
            return Err(format!(
                "invalid emby path mapping on line {}: expected one SOURCE => DESTINATION pair",
                index + 1
            ));
        }
        let source = source.trim();
        let destination = destination.trim();
        if !is_absolute_path(source) || !is_absolute_path(destination) {
            return Err(format!(
                "invalid emby path mapping on line {}: both sides must be absolute paths",
                index + 1
            ));
        }

        let case_insensitive = is_windows_or_unc_path(source);
        let source_prefix = trim_trailing_separator(&normalize_separators(source));
        let destination_separator = if destination.contains('\\') && !destination.contains('/') {
            '\\'
        } else {
            '/'
        };
        let destination_prefix = normalize_destination(destination, destination_separator);
        mappings.push(PathMapping {
            source_prefix,
            destination_prefix,
            destination_separator,
            case_insensitive,
        });
    }

    mappings.sort_by_key(|mapping| Reverse(mapping.source_prefix.len()));
    Ok(mappings)
}

fn map_path(mappings: &[PathMapping], source_path: &str) -> Option<String> {
    let normalized_path = trim_trailing_separator(&normalize_separators(source_path));

    for mapping in mappings {
        if !prefix_matches(
            &mapping.source_prefix,
            &normalized_path,
            mapping.case_insensitive,
        ) {
            continue;
        }

        let suffix = &normalized_path[mapping.source_prefix.len()..];
        let separator = mapping.destination_separator;
        let mut converted_suffix = suffix.replace('/', &separator.to_string());
        if !converted_suffix.is_empty() && !converted_suffix.starts_with(separator) {
            converted_suffix.insert(0, separator);
        }
        if mapping.destination_prefix.ends_with(separator)
            && converted_suffix.starts_with(separator)
        {
            converted_suffix.remove(0);
        }
        return Some(format!(
            "{}{}",
            mapping.destination_prefix, converted_suffix
        ));
    }

    None
}

fn prefix_matches(prefix: &str, full_path: &str, case_insensitive: bool) -> bool {
    let matches = if case_insensitive {
        full_path
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    } else {
        full_path.starts_with(prefix)
    };
    if !matches {
        return false;
    }
    if full_path.len() == prefix.len() || prefix == "/" || prefix.ends_with('/') {
        return true;
    }
    full_path
        .as_bytes()
        .get(prefix.len())
        .is_some_and(|byte| *byte == b'/')
}

fn is_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with("\\\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn is_windows_or_unc_path(value: &str) -> bool {
    value.starts_with("\\\\")
        || (value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic))
}

fn normalize_separators(value: &str) -> String {
    value.replace('\\', "/")
}

fn normalize_destination(value: &str, separator: char) -> String {
    let normalized = if separator == '\\' {
        value.replace('/', "\\")
    } else {
        value.replace('\\', "/")
    };
    trim_trailing_separator(&normalized)
}

fn trim_trailing_separator(value: &str) -> String {
    if value == "/" || value == "\\" {
        return value.to_string();
    }
    value.trim_end_matches(['/', '\\']).to_string()
}

fn build_media_refresh_plan(
    request: &PluginNotificationRequest,
    mappings: &[PathMapping],
) -> Result<MediaRefreshPlan, String> {
    let source_updates = media_updates(request)?;
    let mut updates = Vec::new();
    let mut lookup_update_types = Vec::new();

    if mappings.is_empty() {
        updates = source_updates;
    } else {
        for update in source_updates {
            if let Some(path) = map_path(mappings, &update.path) {
                updates.push(MediaUpdate {
                    path,
                    update_type: update.update_type,
                });
            } else if !lookup_update_types.contains(&update.update_type) {
                lookup_update_types.push(update.update_type);
            }
        }
    }

    dedupe_updates(&mut updates);
    let lookup = if lookup_update_types.is_empty() {
        None
    } else {
        Some(item_lookup(request.title.as_ref().ok_or_else(|| {
            "unmapped Emby paths require title metadata".to_string()
        })?)?)
    };

    Ok(MediaRefreshPlan {
        updates,
        lookup,
        lookup_update_types,
    })
}

fn media_updates(request: &PluginNotificationRequest) -> Result<Vec<MediaUpdate>, String> {
    let mut updates = request
        .file
        .as_ref()
        .map(parse_file_updates)
        .unwrap_or_default();

    if updates.is_empty() {
        let fallback_path = request
            .file
            .as_ref()
            .and_then(|file| file.primary_path.as_deref())
            .or_else(|| {
                request
                    .title
                    .as_ref()
                    .and_then(|title| title.path.as_deref())
            })
            .map(str::trim)
            .filter(|path| !path.is_empty());
        let update_type = event_update_type(request.event_type);
        if let (Some(path), Some(update_type)) = (fallback_path, update_type) {
            updates.push(MediaUpdate {
                path: path.to_string(),
                update_type,
            });
        }
    }

    dedupe_updates(&mut updates);
    if updates.is_empty() {
        Err("Emby notification did not contain any media paths to update".to_string())
    } else {
        Ok(updates)
    }
}

fn parse_file_updates(file: &PluginNotificationFile) -> Vec<MediaUpdate> {
    file.media_updates
        .iter()
        .filter_map(|update| {
            let path = update.path.trim();
            if path.is_empty() {
                return None;
            }
            let update_type = match update.update_type {
                NotificationMediaUpdateType::Created => MediaUpdateType::Created,
                NotificationMediaUpdateType::Modified => MediaUpdateType::Modified,
                NotificationMediaUpdateType::Deleted => MediaUpdateType::Deleted,
            };
            Some(MediaUpdate {
                path: path.to_string(),
                update_type,
            })
        })
        .collect()
}

fn event_update_type(event_type: NotificationEventType) -> Option<MediaUpdateType> {
    match event_type {
        NotificationEventType::ImportComplete => Some(MediaUpdateType::Created),
        NotificationEventType::Upgrade | NotificationEventType::Rename => {
            Some(MediaUpdateType::Modified)
        }
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            Some(MediaUpdateType::Deleted)
        }
        _ => None,
    }
}

fn item_lookup(title: &PluginNotificationTitle) -> Result<ItemLookup, String> {
    let item_type = match title.facet.trim().to_ascii_lowercase().as_str() {
        "series" | "anime" | "tv" => EmbyItemType::Series,
        "movie" => EmbyItemType::Movie,
        other => {
            return Err(format!(
                "unmapped Emby paths have unsupported title facet: {other}"
            ));
        }
    };
    let name = title.name.trim();
    if name.is_empty() {
        return Err("unmapped Emby paths require a title name".to_string());
    }

    Ok(ItemLookup {
        item_type,
        title: name.to_string(),
        year: title.year.map(i64::from),
        external_ids: ExternalIds {
            tvdb_id: trimmed(title.external_ids.tvdb_id.as_deref()),
            imdb_id: trimmed(title.external_ids.imdb_id.as_deref()),
            tmdb_id: trimmed(title.external_ids.tmdb_id.as_deref()),
            tvmaze_id: trimmed(title.external_ids.tvmaze_id.as_deref()),
        },
    })
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn dedupe_updates(updates: &mut Vec<MediaUpdate>) {
    let mut seen = HashSet::new();
    updates.retain(|update| seen.insert((update.path.clone(), update.update_type)));
}

fn execute_media_refresh(mut plan: MediaRefreshPlan, config: &EmbyConfig) -> Result<(), String> {
    if let Some(lookup) = &plan.lookup {
        let body = execute_http(
            build_item_lookup_request(lookup, config),
            "Emby item lookup",
        )?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| "Emby item lookup returned invalid JSON".to_string())?;
        let paths = matching_item_paths(&value, lookup);
        merge_lookup_paths(&mut plan, paths)?;
    }

    dedupe_updates(&mut plan.updates);
    if plan.updates.is_empty() {
        return Err("Emby notification did not resolve any media paths".to_string());
    }
    execute_http(
        build_media_updated_request(&plan.updates, config)?,
        "Emby media update",
    )?;
    Ok(())
}

fn merge_lookup_paths(plan: &mut MediaRefreshPlan, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Err("Emby item lookup found no matching paths".to_string());
    }
    for path in paths {
        for update_type in &plan.lookup_update_types {
            plan.updates.push(MediaUpdate {
                path: path.clone(),
                update_type: *update_type,
            });
        }
    }
    dedupe_updates(&mut plan.updates);
    Ok(())
}

fn build_system_info_request(config: &EmbyConfig) -> PreparedHttpRequest {
    base_request("GET", &format!("{}/System/Info", config.base_url), config)
}

fn build_item_lookup_request(lookup: &ItemLookup, config: &EmbyConfig) -> PreparedHttpRequest {
    let mut params = vec![
        ("Recursive", "true".to_string()),
        ("IncludeItemTypes", lookup.item_type.as_emby().to_string()),
        ("Fields", "Path,ProviderIds".to_string()),
    ];
    if let Some(year) = lookup.year {
        params.push(("Years", year.to_string()));
    }
    let url = append_query(&format!("{}/Items", config.base_url), &params);
    base_request("GET", &url, config)
}

fn build_media_updated_request(
    updates: &[MediaUpdate],
    config: &EmbyConfig,
) -> Result<PreparedHttpRequest, String> {
    let payload_updates = updates
        .iter()
        .map(|update| {
            serde_json::json!({
                "Path": update.path,
                "UpdateType": update.update_type.as_emby(),
            })
        })
        .collect::<Vec<_>>();
    let body = serde_json::to_vec(&serde_json::json!({ "Updates": payload_updates }))
        .map_err(|_| "failed to encode Emby media update payload".to_string())?;
    Ok(base_request(
        "POST",
        &format!("{}/Library/Media/Updated", config.base_url),
        config,
    )
    .with_header("Content-Type", "application/json")
    .with_body(body))
}

fn base_request(method: &str, url: &str, config: &EmbyConfig) -> PreparedHttpRequest {
    PreparedHttpRequest::new(method, url.to_string())
        .with_header("Accept", "application/json")
        .with_header("X-Emby-Token", &config.api_key)
        .with_header("User-Agent", "scryer-emby-plugin/0.1")
}

fn execute_http(prepared: PreparedHttpRequest, operation: &str) -> Result<Vec<u8>, String> {
    let (request, body) = prepared.into_http_request();
    let response = http::request::<Vec<u8>>(&request, body)
        .map_err(|_| format!("{operation} request failed"))?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        return Err(http_status_error(operation, status));
    }
    if response.body().len() > MAX_JSON_RESPONSE_BYTES {
        return Err(format!("{operation} response exceeded the size limit"));
    }
    Ok(response.body())
}

fn http_status_error(operation: &str, status: u16) -> String {
    format!("{operation} failed with HTTP {status}")
}

fn matching_item_paths(value: &serde_json::Value, lookup: &ItemLookup) -> Vec<String> {
    let Some(items) = value
        .get("Items")
        .or_else(|| value.get("items"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };

    for (provider, expected) in provider_id_priorities(lookup) {
        let Some(expected) = expected else {
            continue;
        };
        let paths = unique_paths(items.iter().filter(|item| {
            object_member(item, "ProviderIds").is_some_and(|provider_ids| {
                string_member(provider_ids, provider)
                    .is_some_and(|actual| external_ids_equal(&actual, expected))
            })
        }));
        if !paths.is_empty() {
            return paths;
        }
    }

    unique_paths(
        items
            .iter()
            .filter(|item| string_member(item, "Name").is_some_and(|name| name == lookup.title)),
    )
}

fn provider_id_priorities(lookup: &ItemLookup) -> Vec<(&'static str, Option<&str>)> {
    match lookup.item_type {
        EmbyItemType::Series => vec![
            ("Tvdb", lookup.external_ids.tvdb_id.as_deref()),
            ("Imdb", lookup.external_ids.imdb_id.as_deref()),
            ("Tmdb", lookup.external_ids.tmdb_id.as_deref()),
            ("TvMaze", lookup.external_ids.tvmaze_id.as_deref()),
        ],
        EmbyItemType::Movie => vec![
            ("Tmdb", lookup.external_ids.tmdb_id.as_deref()),
            ("Imdb", lookup.external_ids.imdb_id.as_deref()),
        ],
    }
}

fn unique_paths<'a>(items: impl Iterator<Item = &'a serde_json::Value>) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        if let Some(path) = string_member(item, "Path")
            && seen.insert(path.clone())
        {
            paths.push(path);
        }
    }
    paths
}

fn object_member<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    value
        .as_object()?
        .iter()
        .find_map(|(candidate, value)| candidate.eq_ignore_ascii_case(key).then_some(value))
}

fn string_member(value: &serde_json::Value, key: &str) -> Option<String> {
    object_member(value, key)
        .and_then(|value| match value {
            serde_json::Value::String(value) => Some(value.trim().to_string()),
            serde_json::Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
        .filter(|value| !value.is_empty())
}

fn external_ids_equal(actual: &str, expected: &str) -> bool {
    match (actual.parse::<u64>(), expected.parse::<u64>()) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => actual.eq_ignore_ascii_case(expected),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        PluginNotificationApp, PluginNotificationExternalIds, PluginNotificationMediaUpdate,
    };
    use std::collections::BTreeMap;

    fn config_values(values: &[(&str, &str)]) -> EmbyConfig {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<BTreeMap<_, _>>();
        EmbyConfig::from_lookup(|key| values.get(key).cloned()).unwrap()
    }

    fn test_config(path_mappings: &str) -> EmbyConfig {
        config_values(&[
            ("base_url", "http://emby:8096"),
            ("api_key", "secret"),
            ("path_mappings", path_mappings),
        ])
    }

    fn request(
        event_type: NotificationEventType,
        facet: &str,
        title_path: Option<&str>,
        media_updates: Vec<(&str, NotificationMediaUpdateType)>,
        ids: PluginNotificationExternalIds,
    ) -> PluginNotificationRequest {
        let media_updates = media_updates
            .into_iter()
            .map(|(path, update_type)| PluginNotificationMediaUpdate {
                path: path.to_string(),
                update_type,
            })
            .collect::<Vec<_>>();
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: Some("evt-1".to_string()),
            occurred_at: Some("2026-08-13T12:00:00Z".to_string()),
            correlation_id: Some("corr-1".to_string()),
            actor: None,
            severity: None,
            is_test: false,
            summary_title: "Test".to_string(),
            summary_message: "Body".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "test".to_string(),
            },
            title: Some(PluginNotificationTitle {
                id: None,
                name: "Example Title".to_string(),
                facet: facet.to_string(),
                year: Some(2025),
                slug: None,
                path: title_path.map(str::to_string),
                overview: None,
                sort_title: None,
                background_url: None,
                poster_url: None,
                tags: Vec::new(),
                aliases: Vec::new(),
                original_language: None,
                original_country: None,
                external_ids: ids,
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
        }
    }

    fn ids(
        tvdb: Option<&str>,
        imdb: Option<&str>,
        tmdb: Option<&str>,
        tvmaze: Option<&str>,
    ) -> PluginNotificationExternalIds {
        PluginNotificationExternalIds {
            tmdb_id: tmdb.map(str::to_string),
            imdb_id: imdb.map(str::to_string),
            tvdb_id: tvdb.map(str::to_string),
            anidb_id: None,
            tvmaze_id: tvmaze.map(str::to_string),
            anilist_ids: Vec::new(),
            mal_ids: Vec::new(),
            kitsu_ids: Vec::new(),
            by_source: Default::default(),
        }
    }

    #[test]
    fn descriptor_is_first_class_emby() {
        let descriptor = default_descriptor();
        assert_eq!(descriptor.id, "emby");
        assert_eq!(descriptor.name, "Emby");
        let ProviderDescriptor::Notification(notification) = descriptor.provider else {
            panic!("expected notification descriptor");
        };
        assert_eq!(notification.provider_type, "emby");
        assert_eq!(notification.provider_aliases, vec!["mediabrowser"]);
        assert_eq!(
            notification.capabilities.delivery_modes,
            vec![NotificationDeliveryMode::MediaServerUpdate]
        );
        assert_eq!(
            notification.capabilities.payload_formats,
            vec![NotificationPayloadFormat::StructuredJson]
        );
        assert!(notification.capabilities.supports_batch);
        assert_eq!(
            notification
                .config_fields
                .iter()
                .map(|field| field.key.as_str())
                .collect::<Vec<_>>(),
            vec!["base_url", "api_key", "path_mappings"]
        );
        assert_eq!(
            notification.config_fields[2].field_type,
            ConfigFieldType::Multiline
        );
    }

    #[test]
    fn descriptor_supports_only_targeted_refresh_events() {
        let ProviderDescriptor::Notification(notification) = default_descriptor().provider else {
            panic!("expected notification descriptor");
        };
        assert_eq!(
            notification.capabilities.supported_events,
            media_refresh_events()
        );
        assert!(
            !notification
                .capabilities
                .supported_events
                .contains(&NotificationEventType::TitleAdded)
        );
    }

    #[test]
    fn config_uses_first_class_fields_only() {
        let config = config_values(&[
            ("base_url", " https://emby.example.test/proxy/// "),
            ("api_key", " secret "),
            ("path_mappings", "/data/tv => /media/tv"),
        ]);
        assert_eq!(config.base_url, "https://emby.example.test/proxy");
        assert_eq!(config.api_key, "secret");
        assert_eq!(config.path_mappings.len(), 1);
    }

    #[test]
    fn config_rejects_invalid_urls_and_does_not_accept_legacy_server_fields() {
        let invalid_url = BTreeMap::from([
            ("base_url".to_string(), "ftp://emby".to_string()),
            ("api_key".to_string(), "secret".to_string()),
        ]);
        assert!(EmbyConfig::from_lookup(|key| invalid_url.get(key).cloned()).is_err());

        let credentials_in_url = BTreeMap::from([
            (
                "base_url".to_string(),
                "http://user:password@emby".to_string(),
            ),
            ("api_key".to_string(), "secret".to_string()),
        ]);
        assert!(EmbyConfig::from_lookup(|key| credentials_in_url.get(key).cloned()).is_err());

        let legacy_only = BTreeMap::from([
            ("host".to_string(), "emby".to_string()),
            ("api_key".to_string(), "secret".to_string()),
        ]);
        assert!(EmbyConfig::from_lookup(|key| legacy_only.get(key).cloned()).is_err());

        let config = config_values(&[
            ("base_url", "http://emby:8096"),
            ("api_key", "secret"),
            ("map_from", "/data"),
            ("map_to", "/media"),
        ]);
        assert!(config.path_mappings.is_empty());
    }

    #[test]
    fn path_mappings_use_longest_prefix_and_path_boundaries() {
        let mappings =
            parse_path_mappings("/data/media => /mnt/media\n/data/media/anime => /srv/anime")
                .unwrap();
        assert_eq!(
            map_path(&mappings, "/data/media/anime/Show/E01.mkv").as_deref(),
            Some("/srv/anime/Show/E01.mkv")
        );
        assert_eq!(map_path(&mappings, "/data/media2/Movie.mkv"), None);

        let root_destination = parse_path_mappings("/data => /").unwrap();
        assert_eq!(
            map_path(&root_destination, "/data/Movie.mkv").as_deref(),
            Some("/Movie.mkv")
        );
    }

    #[test]
    fn path_mappings_support_windows_and_unc_paths() {
        let mappings = parse_path_mappings(
            r"C:\Media\TV => D:\Emby\TV
\\nas\anime => /mnt/anime",
        )
        .unwrap();
        assert_eq!(
            map_path(&mappings, r"c:\media\tv\Show\S01E01.mkv").as_deref(),
            Some(r"D:\Emby\TV\Show\S01E01.mkv")
        );
        assert_eq!(
            map_path(&mappings, r"\\NAS\ANIME\Show\E01.mkv").as_deref(),
            Some("/mnt/anime/Show/E01.mkv")
        );
    }

    #[test]
    fn path_mappings_require_absolute_paths_and_cap_rules() {
        assert!(parse_path_mappings("relative => /media").is_err());
        let eleven = (0..11)
            .map(|index| format!("/source/{index} => /destination/{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_path_mappings(&eleven).is_err());
    }

    #[test]
    fn no_mappings_treats_event_paths_as_emby_visible_and_dedupes_pairs() {
        let request = request(
            NotificationEventType::ImportComplete,
            "series",
            None,
            vec![
                (
                    "/media/tv/Show/E01.mkv",
                    NotificationMediaUpdateType::Created,
                ),
                (
                    "/media/tv/Show/E01.mkv",
                    NotificationMediaUpdateType::Created,
                ),
                (
                    "/media/tv/Show/E01.mkv",
                    NotificationMediaUpdateType::Modified,
                ),
            ],
            ids(Some("1"), None, None, None),
        );
        let plan = build_media_refresh_plan(&request, &[]).unwrap();
        assert_eq!(plan.updates.len(), 2);
        assert_eq!(plan.lookup, None);
    }

    #[test]
    fn configured_mappings_plan_item_lookup_only_for_unmapped_paths() {
        let request = request(
            NotificationEventType::Upgrade,
            "movie",
            None,
            vec![
                (
                    "/data/movies/Movie.mkv",
                    NotificationMediaUpdateType::Modified,
                ),
                ("/other/Movie.mkv", NotificationMediaUpdateType::Created),
            ],
            ids(None, Some("tt123"), Some("99"), None),
        );
        let mappings = parse_path_mappings("/data/movies => /media/movies").unwrap();
        let plan = build_media_refresh_plan(&request, &mappings).unwrap();
        assert_eq!(
            plan.updates,
            vec![MediaUpdate {
                path: "/media/movies/Movie.mkv".to_string(),
                update_type: MediaUpdateType::Modified,
            }]
        );
        assert_eq!(plan.lookup.as_ref().unwrap().item_type, EmbyItemType::Movie);
        assert_eq!(plan.lookup_update_types, vec![MediaUpdateType::Created]);
    }

    #[test]
    fn lookup_results_merge_into_one_deduplicated_update_batch() {
        let mut plan = MediaRefreshPlan {
            updates: vec![MediaUpdate {
                path: "/media/movies/Movie.mkv".to_string(),
                update_type: MediaUpdateType::Modified,
            }],
            lookup: None,
            lookup_update_types: vec![MediaUpdateType::Modified, MediaUpdateType::Deleted],
        };
        merge_lookup_paths(
            &mut plan,
            vec![
                "/media/movies/Movie.mkv".to_string(),
                "/media/movies/Movie.mkv".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            plan.updates,
            vec![
                MediaUpdate {
                    path: "/media/movies/Movie.mkv".to_string(),
                    update_type: MediaUpdateType::Modified,
                },
                MediaUpdate {
                    path: "/media/movies/Movie.mkv".to_string(),
                    update_type: MediaUpdateType::Deleted,
                },
            ]
        );
    }

    #[test]
    fn title_path_fallback_maps_events_to_exact_emby_update_types() {
        for (event, expected) in [
            (
                NotificationEventType::ImportComplete,
                MediaUpdateType::Created,
            ),
            (NotificationEventType::Upgrade, MediaUpdateType::Modified),
            (NotificationEventType::Rename, MediaUpdateType::Modified),
            (NotificationEventType::FileDeleted, MediaUpdateType::Deleted),
            (
                NotificationEventType::FileDeletedForUpgrade,
                MediaUpdateType::Deleted,
            ),
        ] {
            let request = request(
                event,
                "movie",
                Some("/media/Movie.mkv"),
                vec![],
                ids(None, None, None, None),
            );
            let updates = media_updates(&request).unwrap();
            assert_eq!(updates[0].update_type, expected);
        }
    }

    #[test]
    fn lookup_request_uses_exact_emby_query_and_token_header() {
        let config = test_config("");
        let lookup = ItemLookup {
            item_type: EmbyItemType::Series,
            title: "Show".to_string(),
            year: Some(2025),
            external_ids: ExternalIds::default(),
        };
        let request = build_item_lookup_request(&lookup, &config);
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url,
            "http://emby:8096/Items?Recursive=true&IncludeItemTypes=Series&Fields=Path%2CProviderIds&Years=2025"
        );
        assert_eq!(request.header_value("Accept"), Some("application/json"));
        assert_eq!(request.header_value("X-Emby-Token"), Some("secret"));
        assert_eq!(request.body, None);
    }

    #[test]
    fn series_lookup_uses_provider_precedence_before_title_fallback() {
        let lookup = ItemLookup {
            item_type: EmbyItemType::Series,
            title: "Example Title".to_string(),
            year: None,
            external_ids: ExternalIds {
                tvdb_id: Some("123".to_string()),
                imdb_id: Some("tt456".to_string()),
                tmdb_id: None,
                tvmaze_id: None,
            },
        };
        let response = serde_json::json!({
            "Items": [
                { "Name": "Example Title", "Path": "/by-name", "ProviderIds": {} },
                { "Name": "Wrong", "Path": "/by-imdb", "ProviderIds": { "Imdb": "tt456" } },
                { "Name": "Wrong", "Path": "/by-tvdb", "ProviderIds": { "Tvdb": 123 } }
            ]
        });
        assert_eq!(matching_item_paths(&response, &lookup), vec!["/by-tvdb"]);

        let no_id_match = serde_json::json!({
            "Items": [
                { "Name": "Example Title", "Path": "/by-name", "ProviderIds": {} },
                { "Name": "Other", "Path": "/other", "ProviderIds": {} }
            ]
        });
        assert_eq!(matching_item_paths(&no_id_match, &lookup), vec!["/by-name"]);
    }

    #[test]
    fn movie_lookup_prefers_tmdb_and_collects_all_matching_paths() {
        let lookup = ItemLookup {
            item_type: EmbyItemType::Movie,
            title: "Movie".to_string(),
            year: None,
            external_ids: ExternalIds {
                tmdb_id: Some("99".to_string()),
                imdb_id: Some("tt99".to_string()),
                ..ExternalIds::default()
            },
        };
        let response = serde_json::json!({
            "Items": [
                { "Path": "/movie-a", "ProviderIds": { "Tmdb": "099" } },
                { "Path": "/movie-b", "ProviderIds": { "tmdb": 99 } },
                { "Path": "/imdb-only", "ProviderIds": { "Imdb": "tt99" } }
            ]
        });
        assert_eq!(
            matching_item_paths(&response, &lookup),
            vec!["/movie-a", "/movie-b"]
        );
    }

    #[test]
    fn system_info_test_uses_exact_endpoint_and_headers() {
        let request = build_system_info_request(&test_config(""));
        assert_eq!(request.method, "GET");
        assert_eq!(request.url, "http://emby:8096/System/Info");
        assert_eq!(request.header_value("Accept"), Some("application/json"));
        assert_eq!(request.header_value("X-Emby-Token"), Some("secret"));
        assert_eq!(request.body, None);
    }

    #[test]
    fn media_update_batches_and_uses_exact_pascal_case_payload() {
        let updates = vec![
            MediaUpdate {
                path: "/media/a.mkv".to_string(),
                update_type: MediaUpdateType::Created,
            },
            MediaUpdate {
                path: "/media/b.mkv".to_string(),
                update_type: MediaUpdateType::Deleted,
            },
        ];
        let request = build_media_updated_request(&updates, &test_config("")).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "http://emby:8096/Library/Media/Updated");
        assert_eq!(request.header_value("Accept"), Some("application/json"));
        assert_eq!(request.header_value("X-Emby-Token"), Some("secret"));
        assert_eq!(
            request.header_value("Content-Type"),
            Some("application/json")
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "Updates": [
                    { "Path": "/media/a.mkv", "UpdateType": "Created" },
                    { "Path": "/media/b.mkv", "UpdateType": "Deleted" }
                ]
            })
        );
    }

    #[test]
    fn errors_do_not_echo_response_bodies_or_api_keys() {
        let error = http_status_error("Emby item lookup", 401);
        assert_eq!(error, "Emby item lookup failed with HTTP 401");
        assert!(!error.contains("secret"));
        assert!(!error.contains("response body"));
    }
}
