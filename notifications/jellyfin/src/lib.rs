use std::collections::{HashMap, HashSet};

use extism_pdk::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct PluginDescriptor {
    name: String,
    version: String,
    sdk_version: String,
    plugin_type: String,
    provider_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_aliases: Vec<String>,
    capabilities: IndexerCapabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scoring_policies: Vec<()>,
    config_fields: Vec<ConfigFieldDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notification_capabilities: Option<NotificationCapabilities>,
}

#[derive(Serialize)]
struct IndexerCapabilities {
    search: bool,
    imdb_search: bool,
    tvdb_search: bool,
}

#[derive(Serialize)]
struct NotificationCapabilities {
    supports_rich_text: bool,
    supports_images: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_events: Vec<String>,
}

#[derive(Serialize)]
struct ConfigFieldDef {
    key: String,
    label: String,
    field_type: String,
    #[serde(default)]
    required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    options: Vec<ConfigFieldOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    help_text: Option<String>,
}

#[derive(Serialize)]
struct ConfigFieldOption {
    value: String,
    label: String,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct PluginNotificationRequest {
    event_type: String,
    title: String,
    message: String,
    #[serde(default)]
    title_name: Option<String>,
    #[serde(default)]
    title_year: Option<i32>,
    #[serde(default)]
    title_facet: Option<String>,
    #[serde(default)]
    poster_url: Option<String>,
    #[serde(default)]
    episode_info: Option<String>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    release_title: Option<String>,
    #[serde(default)]
    download_client: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    health_message: Option<String>,
    #[serde(default)]
    application_version: Option<String>,
    #[serde(default)]
    metadata: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct PluginNotificationResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct JellyfinConfig {
    base_url: String,
    api_key: String,
    path_mappings: String,
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
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "created" => Some(Self::Created),
            "modified" => Some(Self::Modified),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

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

fn default_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: "Jellyfin".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "notification".to_string(),
        provider_type: "jellyfin".to_string(),
        provider_aliases: vec![],
        capabilities: IndexerCapabilities {
            search: false,
            imdb_search: false,
            tvdb_search: false,
        },
        scoring_policies: vec![],
        config_fields: vec![
            ConfigFieldDef {
                key: "base_url".to_string(),
                label: "Base URL".to_string(),
                field_type: "string".to_string(),
                required: true,
                default_value: None,
                options: vec![],
                help_text: Some(
                    "Jellyfin server URL, for example http://jellyfin:8096".to_string(),
                ),
            },
            ConfigFieldDef {
                key: "api_key".to_string(),
                label: "API Key".to_string(),
                field_type: "password".to_string(),
                required: true,
                default_value: None,
                options: vec![],
                help_text: Some("Jellyfin API key used for targeted refresh calls.".to_string()),
            },
            ConfigFieldDef {
                key: "path_mappings".to_string(),
                label: "Path Mappings".to_string(),
                field_type: "multiline".to_string(),
                required: true,
                default_value: None,
                options: vec![],
                help_text: Some(
                    "One mapping per line: /scryer/path => /jellyfin/path. Longest prefix wins."
                        .to_string(),
                ),
            },
        ],
        allowed_hosts: vec![],
        notification_capabilities: Some(NotificationCapabilities {
            supports_rich_text: false,
            supports_images: false,
            supported_events: vec![
                "download".to_string(),
                "import_complete".to_string(),
                "upgrade".to_string(),
                "rename".to_string(),
                "file_deleted".to_string(),
                "file_deleted_for_upgrade".to_string(),
                "test".to_string(),
            ],
        }),
    }
}

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&default_descriptor())?)
}

#[plugin_fn]
pub fn send_notification(input: String) -> FnResult<String> {
    let request: PluginNotificationRequest = serde_json::from_str(&input)?;

    let config = match JellyfinConfig::from_extism() {
        Ok(config) => config,
        Err(error) => return Ok(error_response(error)),
    };

    let plans = match build_request_plans(&request, &config) {
        Ok(plans) => plans,
        Err(error) => return Ok(error_response(error)),
    };

    for plan in &plans {
        if let Err(error) = execute_plan(plan, &config) {
            return Ok(error_response(error));
        }
    }

    Ok(success_response())
}

impl JellyfinConfig {
    fn from_extism() -> Result<Self, String> {
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

        Ok(Self {
            base_url,
            api_key,
            path_mappings,
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
) -> Result<Vec<JellyfinRequestPlan>, String> {
    if request.event_type == "test" {
        return Ok(vec![JellyfinRequestPlan::SystemInfo]);
    }

    let mappings = parse_path_mappings(&config.path_mappings)?;
    let updates = parse_media_updates(&request.metadata)?;
    let external_ids = parse_external_ids(&request.metadata);
    let title_facet = request
        .title_facet
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            request
                .metadata
                .get("title_facet")
                .and_then(serde_json::Value::as_str)
        });

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
                    return Err(
                        "found unmapped media updates but no tmdb_id/imdb_id fallback for movie"
                            .to_string(),
                    );
                }
            }
            "series" | "anime" | "tv" => {
                if let Some(tvdb_id) = external_ids.tvdb_id {
                    plans.push(JellyfinRequestPlan::SeriesUpdated { tvdb_id });
                } else {
                    return Err(
                        "found unmapped media updates but no tvdb_id fallback for series/anime"
                            .to_string(),
                    );
                }
            }
            other => {
                return Err(format!(
                    "found unmapped media updates but unsupported or missing title_facet: {other}"
                ));
            }
        }
    }

    if plans.is_empty() {
        return Err("notification did not contain any media updates to send".to_string());
    }

    Ok(plans)
}

fn parse_media_updates(
    metadata: &HashMap<String, serde_json::Value>,
) -> Result<Vec<MediaUpdate>, String> {
    let Some(value) = metadata.get("media_updates") else {
        return Err("metadata.media_updates is required".to_string());
    };
    let updates = value
        .as_array()
        .ok_or_else(|| "metadata.media_updates must be an array".to_string())?;

    let mut parsed = Vec::with_capacity(updates.len());
    for item in updates {
        let path = item
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "media_updates[].path is required".to_string())?;
        let update_type = item
            .get("update_type")
            .and_then(serde_json::Value::as_str)
            .and_then(MediaUpdateType::parse)
            .ok_or_else(|| {
                "media_updates[].update_type must be created, modified, or deleted".to_string()
            })?;
        parsed.push(MediaUpdate {
            path: path.to_string(),
            update_type,
        });
    }

    Ok(parsed)
}

fn parse_external_ids(metadata: &HashMap<String, serde_json::Value>) -> ExternalIds {
    let Some(value) = metadata.get("external_ids") else {
        return ExternalIds::default();
    };
    let Some(object) = value.as_object() else {
        return ExternalIds::default();
    };

    ExternalIds {
        tmdb_id: object
            .get("tmdb_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        imdb_id: object
            .get("imdb_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        tvdb_id: object
            .get("tvdb_id")
            .and_then(serde_json::Value::as_str)
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

fn execute_plan(plan: &JellyfinRequestPlan, config: &JellyfinConfig) -> Result<(), String> {
    let request = build_http_request(plan, config)?;
    execute_http(request)
}

fn build_http_request(
    plan: &JellyfinRequestPlan,
    config: &JellyfinConfig,
) -> Result<PreparedHttpRequest, String> {
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
            let body = serde_json::to_vec(&payload)
                .map_err(|error| format!("failed to encode media update payload: {error}"))?;
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
    }
}

fn base_request(method: &str, url: &str, config: &JellyfinConfig) -> PreparedHttpRequest {
    PreparedHttpRequest::new(method, url.to_string())
        .with_header(
            "Authorization",
            &format!("MediaBrowser Token=\"{}\"", config.api_key),
        )
        .with_header("Accept", "application/json")
        .with_header("User-Agent", "scryer-jellyfin-plugin/0.1")
}

fn execute_http(prepared: PreparedHttpRequest) -> Result<(), String> {
    let (request, body) = prepared.into_http_request();
    match http::request::<Vec<u8>>(&request, body) {
        Ok(response) => {
            let status = response.status_code();
            if (200..300).contains(&status) {
                Ok(())
            } else {
                let text = String::from_utf8_lossy(&response.body()).to_string();
                Err(format!("HTTP {}: {}", status, text))
            }
        }
        Err(error) => Err(format!("request failed: {error}")),
    }
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

fn success_response() -> String {
    serde_json::to_string(&PluginNotificationResponse {
        success: true,
        error: None,
    })
    .unwrap_or_else(|_| "{\"success\":true}".to_string())
}

fn error_response(error: String) -> String {
    serde_json::to_string(&PluginNotificationResponse {
        success: false,
        error: Some(error),
    })
    .unwrap_or_else(|_| "{\"success\":false,\"error\":\"notification failed\"}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_metadata(
        event_type: &str,
        title_facet: &str,
        metadata: serde_json::Value,
    ) -> PluginNotificationRequest {
        let metadata = metadata
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        PluginNotificationRequest {
            event_type: event_type.to_string(),
            title: "Test".to_string(),
            message: "Body".to_string(),
            title_name: Some("Test".to_string()),
            title_year: None,
            title_facet: Some(title_facet.to_string()),
            poster_url: None,
            episode_info: None,
            quality: None,
            release_title: None,
            download_client: None,
            file_path: None,
            health_message: None,
            application_version: None,
            metadata,
        }
    }

    fn config(path_mappings: &str) -> JellyfinConfig {
        JellyfinConfig {
            base_url: "http://jellyfin:8096".to_string(),
            api_key: "secret".to_string(),
            path_mappings: path_mappings.to_string(),
        }
    }

    #[test]
    fn describe_includes_expected_fields() {
        let descriptor = default_descriptor();
        let json = serde_json::to_value(descriptor).unwrap();
        assert_eq!(json["provider_type"], "jellyfin");
        assert_eq!(json["plugin_type"], "notification");
        assert_eq!(json["config_fields"][2]["field_type"], "multiline");
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
        assert!(error.contains("tmdb_id/imdb_id"));
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
        assert_eq!(request.header_value("Content-Type"), Some("application/json"));

        let body: serde_json::Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
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
}
