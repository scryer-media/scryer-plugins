use std::collections::{BTreeSet, HashMap};

use extism_pdk::*;
use scryer_plugin_sdk::{
    current_sdk_constraint, ConfigFieldDef, ConfigFieldRole, ConfigFieldType,
    IndexerCapabilities as Capabilities, IndexerDescriptor, IndexerManagementCapabilities,
    IndexerPluginSyncPlanRequest, IndexerPluginSyncPlanResponse,
    IndexerPluginValidateConfigRequest, IndexerPluginValidateConfigResponse, IndexerSourceKind,
    IndexerValidateConfigStatus, ManagedIndexerChildSpec, ManagedIndexerRoutingScope,
    PluginDescriptor, PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
};
use serde::{Deserialize, Serialize};

const USER_AGENT: &str = "scryer-prowlarr-plugin/0.1";

#[derive(Debug, Clone)]
struct ProwlarrConfig {
    base_url: String,
    api_key: String,
}

impl ProwlarrConfig {
    fn from_extism() -> Result<Self, String> {
        let base_url = config::get("base_url")
            .map_err(|error| format!("missing config base_url: {error}"))?
            .unwrap_or_default();
        let api_key = config::get("api_key")
            .map_err(|error| format!("missing config api_key: {error}"))?
            .unwrap_or_default();

        let base_url = base_url.trim().trim_end_matches('/').to_string();
        let api_key = api_key.trim().to_string();

        if base_url.is_empty() {
            return Err("Prowlarr requires a base_url".to_string());
        }
        if api_key.is_empty() {
            return Err("Prowlarr requires an api_key".to_string());
        }

        Ok(Self { base_url, api_key })
    }
}

#[derive(Debug)]
enum RequestError {
    InvalidConfig(String),
    AuthFailed(String),
    RateLimited(String, Option<i64>),
    Unreachable(String),
    Unsupported(String),
}

impl RequestError {
    fn to_validate_response(&self) -> IndexerPluginValidateConfigResponse {
        match self {
            Self::InvalidConfig(message) => validate_response(
                IndexerValidateConfigStatus::InvalidConfig,
                Some(message.clone()),
                None,
            ),
            Self::AuthFailed(message) => validate_response(
                IndexerValidateConfigStatus::AuthFailed,
                Some(message.clone()),
                None,
            ),
            Self::RateLimited(message, retry_after_seconds) => validate_response(
                IndexerValidateConfigStatus::RateLimited,
                Some(message.clone()),
                *retry_after_seconds,
            ),
            Self::Unreachable(message) => validate_response(
                IndexerValidateConfigStatus::Unreachable,
                Some(message.clone()),
                None,
            ),
            Self::Unsupported(message) => validate_response(
                IndexerValidateConfigStatus::Unsupported,
                Some(message.clone()),
                None,
            ),
        }
    }

    fn into_error(self) -> Error {
        match self {
            Self::InvalidConfig(message)
            | Self::AuthFailed(message)
            | Self::Unreachable(message)
            | Self::Unsupported(message) => Error::msg(message),
            Self::RateLimited(message, retry_after_seconds) => match retry_after_seconds {
                Some(retry_after_seconds) => {
                    Error::msg(format!("{message} (retry after {retry_after_seconds}s)"))
                }
                None => Error::msg(message),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProwlarrSystemStatus {
    version: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ProwlarrIndexerResource {
    id: i64,
    name: String,
    #[serde(default, rename = "enable")]
    enable: bool,
    #[serde(default, rename = "appProfileId")]
    app_profile_id: i64,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    capabilities: ProwlarrIndexerCapabilities,
    #[serde(default)]
    priority: i64,
    #[serde(default, rename = "downloadClientId")]
    download_client_id: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProwlarrIndexerCapabilities {
    #[serde(default)]
    categories: Vec<ProwlarrCategory>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProwlarrCategory {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "subCategories")]
    sub_categories: Vec<ProwlarrCategory>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProwlarrAppProfile {
    id: i64,
    #[serde(default = "default_true", rename = "enableRss")]
    enable_rss: bool,
    #[serde(default = "default_true", rename = "enableAutomaticSearch")]
    enable_automatic_search: bool,
    #[serde(default = "default_true", rename = "enableInteractiveSearch")]
    enable_interactive_search: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManagedChildMetadata {
    indexer_id: i64,
    protocol: String,
    app_profile_id: i64,
    priority: i64,
    download_client_id: i64,
    enable_rss: bool,
    enable_automatic_search: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RoutingScope {
    Movie,
    Series,
    Anime,
}

impl RoutingScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
            Self::Anime => "anime",
        }
    }
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_indexer_search(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::<serde_json::Value>::Err(PluginError {
            code: PluginErrorCode::Unsupported,
            public_message:
                "Prowlarr parent configs are management-only; search through synced child indexers"
                    .to_string(),
            debug_message: None,
            retry_after_seconds: None,
        }),
    )?)
}

#[plugin_fn]
pub fn scryer_indexer_validate_config(input: String) -> FnResult<String> {
    let _: IndexerPluginValidateConfigRequest = serde_json::from_str(&input)?;
    let config = match ProwlarrConfig::from_extism() {
        Ok(config) => config,
        Err(message) => {
            return Ok(serde_json::to_string(&validate_response(
                IndexerValidateConfigStatus::InvalidConfig,
                Some(message),
                None,
            ))?)
        }
    };

    let response = match fetch_system_status(&config) {
        Ok(status) => validate_response(
            IndexerValidateConfigStatus::Valid,
            Some(format!("Connected to Prowlarr {}", status.version)),
            None,
        ),
        Err(error) => error.to_validate_response(),
    };

    Ok(serde_json::to_string(&response)?)
}

#[plugin_fn]
pub fn scryer_indexer_plan_sync(input: String) -> FnResult<String> {
    let _: IndexerPluginSyncPlanRequest = serde_json::from_str(&input)?;
    let config = ProwlarrConfig::from_extism().map_err(Error::msg)?;

    let indexers: Vec<ProwlarrIndexerResource> =
        get_json(&config, "/api/v1/indexer").map_err(RequestError::into_error)?;
    let app_profiles: Vec<ProwlarrAppProfile> =
        get_json(&config, "/api/v1/profiles/app").map_err(RequestError::into_error)?;
    let app_profiles_by_id = app_profiles
        .into_iter()
        .map(|profile| (profile.id, profile))
        .collect::<HashMap<_, _>>();

    let children = indexers
        .into_iter()
        .filter_map(|indexer| build_managed_child_spec(&config, indexer, &app_profiles_by_id))
        .collect::<Vec<_>>();

    Ok(serde_json::to_string(&IndexerPluginSyncPlanResponse {
        children,
    })?)
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "prowlarr".to_string(),
        name: "Prowlarr".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "prowlarr".to_string(),
            provider_aliases: vec![],
            source_kind: IndexerSourceKind::Generic,
            capabilities: Capabilities::default(),
            management_capabilities: IndexerManagementCapabilities {
                supports_validate_config: true,
                supports_managed_children_sync: true,
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        ConfigFieldDef {
            key: "base_url".to_string(),
            label: "Base URL".to_string(),
            field_type: ConfigFieldType::String,
            required: true,
            default_value: None,
            value_source: Default::default(),
            role: Some(ConfigFieldRole::ConnectionUrl),
            host_binding: None,
            options: vec![],
            help_text: Some("Prowlarr server URL, for example http://prowlarr:9696".to_string()),
        },
        ConfigFieldDef {
            key: "api_key".to_string(),
            label: "API Key".to_string(),
            field_type: ConfigFieldType::Password,
            required: true,
            default_value: None,
            value_source: Default::default(),
            role: None,
            host_binding: None,
            options: vec![],
            help_text: Some("Prowlarr API key".to_string()),
        },
    ]
}

fn validate_response(
    status: IndexerValidateConfigStatus,
    message: Option<String>,
    retry_after_seconds: Option<i64>,
) -> IndexerPluginValidateConfigResponse {
    IndexerPluginValidateConfigResponse {
        status,
        message,
        retry_after_seconds,
    }
}

fn fetch_system_status(config: &ProwlarrConfig) -> Result<ProwlarrSystemStatus, RequestError> {
    get_json(config, "/api/v1/system/status")
}

fn get_json<T>(config: &ProwlarrConfig, path: &str) -> Result<T, RequestError>
where
    T: for<'de> Deserialize<'de>,
{
    let request = HttpRequest::new(api_url(&config.base_url, path))
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT)
        .with_header("X-Api-Key", &config.api_key);

    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| RequestError::Unreachable(format!("request failed: {error}")))?;
    let status = response.status_code();
    let body = response.body();
    let body_text = String::from_utf8_lossy(&body).trim().to_string();

    if (200..300).contains(&status) {
        return serde_json::from_slice(&body).map_err(|error| {
            RequestError::Unsupported(format!("Prowlarr returned invalid JSON: {error}"))
        });
    }

    let retry_after_seconds = retry_after_seconds(&response);
    match status {
        400 => Err(RequestError::InvalidConfig(non_empty_or(
            body_text,
            "Prowlarr rejected the request as invalid",
        ))),
        401 | 403 => Err(RequestError::AuthFailed(non_empty_or(
            body_text,
            "Prowlarr rejected the API key",
        ))),
        404 => Err(RequestError::InvalidConfig(non_empty_or(
            body_text,
            "base_url does not appear to point at a Prowlarr API",
        ))),
        429 => Err(RequestError::RateLimited(
            non_empty_or(body_text, "Prowlarr rate limited the request"),
            retry_after_seconds,
        )),
        500..=599 => Err(RequestError::Unreachable(non_empty_or(
            body_text,
            &format!("Prowlarr returned HTTP {status}"),
        ))),
        _ => Err(RequestError::Unsupported(non_empty_or(
            body_text,
            &format!("Prowlarr returned HTTP {status}"),
        ))),
    }
}

fn build_managed_child_spec(
    config: &ProwlarrConfig,
    indexer: ProwlarrIndexerResource,
    app_profiles_by_id: &HashMap<i64, ProwlarrAppProfile>,
) -> Option<ManagedIndexerChildSpec> {
    let provider_type = provider_type_for_protocol(&indexer.protocol)?;
    let app_profile = app_profiles_by_id.get(&indexer.app_profile_id);
    let enable_rss = app_profile
        .map(|profile| profile.enable_rss)
        .unwrap_or(true);
    let enable_automatic_search = app_profile
        .map(|profile| profile.enable_automatic_search)
        .unwrap_or(true);
    let enable_interactive_search = app_profile
        .map(|profile| profile.enable_interactive_search)
        .unwrap_or(true);
    let name = indexer.name.trim();
    let name = if name.is_empty() {
        format!("Prowlarr indexer {}", indexer.id)
    } else {
        name.to_string()
    };
    let routing_categories = collect_routing_categories(&indexer.capabilities.categories);

    let routing_scopes = [
        RoutingScope::Movie,
        RoutingScope::Series,
        RoutingScope::Anime,
    ]
    .into_iter()
    .filter_map(|scope| {
        routing_categories
            .get(scope.as_str())
            .map(|categories| ManagedIndexerRoutingScope {
                scope_id: scope.as_str().to_string(),
                categories: categories.clone(),
            })
    })
    .collect::<Vec<_>>();

    let config_json = serde_json::json!({
        "base_url": config.base_url,
        "api_key": config.api_key,
        "api_path": format!("/api/v1/indexer/{}/newznab", indexer.id),
    });
    let managed_metadata_json = serde_json::to_string(&ManagedChildMetadata {
        indexer_id: indexer.id,
        protocol: indexer.protocol.clone(),
        app_profile_id: indexer.app_profile_id,
        priority: indexer.priority,
        download_client_id: indexer.download_client_id,
        enable_rss,
        enable_automatic_search,
    })
    .ok();

    Some(ManagedIndexerChildSpec {
        child_key: indexer.id.to_string(),
        name,
        provider_type: provider_type.to_string(),
        config_json: serde_json::to_string(&config_json).ok()?,
        is_enabled: indexer.enable,
        enable_interactive_search,
        enable_auto_search: enable_rss || enable_automatic_search,
        managed_metadata_json,
        routing_scopes,
    })
}

fn provider_type_for_protocol(protocol: &str) -> Option<&'static str> {
    match protocol.trim().to_ascii_lowercase().as_str() {
        "usenet" => Some("newznab"),
        "torrent" => Some("torznab"),
        _ => None,
    }
}

fn collect_routing_categories(categories: &[ProwlarrCategory]) -> HashMap<String, Vec<String>> {
    let mut routing = HashMap::<RoutingScope, BTreeSet<String>>::new();
    for category in categories {
        collect_routing_category(category, None, &mut routing);
    }

    routing
        .into_iter()
        .map(|(scope, categories)| (scope.as_str().to_string(), categories.into_iter().collect()))
        .collect()
}

fn collect_routing_category(
    category: &ProwlarrCategory,
    inherited_scope: Option<RoutingScope>,
    routing: &mut HashMap<RoutingScope, BTreeSet<String>>,
) {
    let scope = classify_scope(category).or(inherited_scope);
    if let Some(scope) = scope {
        routing
            .entry(scope)
            .or_default()
            .insert(category.id.to_string());
    }

    for sub_category in &category.sub_categories {
        collect_routing_category(sub_category, scope, routing);
    }
}

fn classify_scope(category: &ProwlarrCategory) -> Option<RoutingScope> {
    let name = category.name.trim().to_ascii_lowercase();
    if name.contains("anime") {
        return Some(RoutingScope::Anime);
    }
    if (2000..3000).contains(&category.id) || name.contains("movie") {
        return Some(RoutingScope::Movie);
    }
    if (5000..6000).contains(&category.id) || name == "tv" || name.contains("series") {
        return Some(RoutingScope::Series);
    }
    None
}

fn api_url(base_url: &str, path: &str) -> String {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

fn retry_after_seconds(response: &HttpResponse) -> Option<i64> {
    response
        .headers()
        .get("retry-after")
        .or_else(|| response.headers().get("x-retry-after"))
        .and_then(|value| value.parse::<i64>().ok())
}

fn non_empty_or(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anime_subcategories_do_not_leak_into_series_routing() {
        let routing = collect_routing_categories(&[ProwlarrCategory {
            id: 5000,
            name: "TV".to_string(),
            sub_categories: vec![
                ProwlarrCategory {
                    id: 5030,
                    name: "TV/HD".to_string(),
                    sub_categories: vec![],
                },
                ProwlarrCategory {
                    id: 5070,
                    name: "TV/Anime".to_string(),
                    sub_categories: vec![],
                },
            ],
        }]);

        assert_eq!(
            routing.get("series"),
            Some(&vec!["5000".to_string(), "5030".to_string()])
        );
        assert_eq!(routing.get("anime"), Some(&vec!["5070".to_string()]));
    }

    #[test]
    fn managed_child_uses_proxy_path_and_app_profile_flags() {
        let config = ProwlarrConfig {
            base_url: "https://prowlarr.example".to_string(),
            api_key: "secret".to_string(),
        };
        let indexer = ProwlarrIndexerResource {
            id: 7,
            name: "Indexer Seven".to_string(),
            enable: true,
            app_profile_id: 12,
            protocol: "torrent".to_string(),
            capabilities: ProwlarrIndexerCapabilities {
                categories: vec![ProwlarrCategory {
                    id: 2000,
                    name: "Movies".to_string(),
                    sub_categories: vec![],
                }],
            },
            priority: 25,
            download_client_id: 3,
        };
        let app_profiles = HashMap::from([(
            12,
            ProwlarrAppProfile {
                id: 12,
                enable_rss: true,
                enable_automatic_search: false,
                enable_interactive_search: true,
            },
        )]);

        let child = build_managed_child_spec(&config, indexer, &app_profiles).expect("child spec");
        let config_json: serde_json::Value = serde_json::from_str(&child.config_json).unwrap();
        let metadata: ManagedChildMetadata =
            serde_json::from_str(child.managed_metadata_json.as_deref().unwrap()).unwrap();

        assert_eq!(child.provider_type, "torznab");
        assert_eq!(config_json["base_url"], "https://prowlarr.example");
        assert_eq!(config_json["api_key"], "secret");
        assert_eq!(config_json["api_path"], "/api/v1/indexer/7/newznab");
        assert!(child.is_enabled);
        assert!(child.enable_interactive_search);
        assert!(child.enable_auto_search);
        assert_eq!(child.routing_scopes.len(), 1);
        assert_eq!(child.routing_scopes[0].scope_id, "movie");
        assert_eq!(child.routing_scopes[0].categories, vec!["2000"]);
        assert_eq!(metadata.indexer_id, 7);
        assert_eq!(metadata.app_profile_id, 12);
        assert_eq!(metadata.download_client_id, 3);
        assert!(metadata.enable_rss);
        assert!(!metadata.enable_automatic_search);
    }

    #[test]
    fn managed_child_keeps_interactive_access_when_rss_is_disabled() {
        let config = ProwlarrConfig {
            base_url: "https://prowlarr.example".to_string(),
            api_key: "secret".to_string(),
        };
        let indexer = ProwlarrIndexerResource {
            id: 9,
            name: "Interactive Only".to_string(),
            enable: true,
            app_profile_id: 21,
            protocol: "torrent".to_string(),
            capabilities: ProwlarrIndexerCapabilities::default(),
            priority: 10,
            download_client_id: 0,
        };
        let app_profiles = HashMap::from([(
            21,
            ProwlarrAppProfile {
                id: 21,
                enable_rss: false,
                enable_automatic_search: false,
                enable_interactive_search: true,
            },
        )]);

        let child = build_managed_child_spec(&config, indexer, &app_profiles).expect("child spec");
        let metadata: ManagedChildMetadata =
            serde_json::from_str(child.managed_metadata_json.as_deref().unwrap()).unwrap();

        assert!(child.is_enabled);
        assert!(child.enable_interactive_search);
        assert!(!child.enable_auto_search);
        assert!(!metadata.enable_rss);
        assert!(!metadata.enable_automatic_search);
    }
}
