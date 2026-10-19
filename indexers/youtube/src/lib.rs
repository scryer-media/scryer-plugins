//! YouTube indexer: searches the YouTube Data API v3 with the operator's own
//! key and returns HTTP-family releases whose only resource is the video's
//! watch page. A download client that declares `provider_page` (MeTube, for
//! example) resolves the media itself.

mod api;
mod cache;
mod cursor;
#[cfg(test)]
mod embed;
mod engine;
mod release;
mod text;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use scryer_plugin_pdk::component::{self, HostError, structured_plugin_error};
use scryer_plugin_pdk::log::{LogLevel, log};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldType, ConfigFieldValueSource, HttpTransport,
    IndexerCapabilities, IndexerDescriptor, IndexerFeedMode, IndexerHttpCapabilities,
    IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures,
    IndexerSearchIncompleteReason, IndexerSearchInput, IndexerSearchPluginError, IndexerSourceKind,
    IndexerStrategyPlanCapability, PluginDescriptor, PluginError, PluginErrorCode,
    PluginErrorDetails, PluginSearchRequest as SearchRequest,
    PluginSearchResponse as SearchResponse, ProviderDescriptor, SDK_VERSION,
    current_sdk_constraint,
};

use crate::api::{API_HOST, API_KEY_HEADER, ApiCall, MAX_RESULTS, YoutubeConfig, YoutubeError};
use crate::engine::{Failure, Fetch, HttpReply, PageOutcome};

const PLUGIN_ID: &str = "youtube";
const USER_AGENT: &str = "Scryer YouTube Indexer/0.1";
/// Seconds between calls the host should keep for this indexer.
const RATE_LIMIT_SECONDS: u32 = 1;

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "YouTube".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        settings: Vec::new(),
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: PLUGIN_ID.to_string(),
            provider_aliases: vec![],
            provider_profiles: vec![],
            search_semantics_version: None,
            // Strategies run one at a time so a plan never bursts the
            // search quota. A paged indexer is searched strategy by strategy
            // by the host anyway.
            strategy_plan: Some(IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 1,
            }),
            source_kind: IndexerSourceKind::Http,
            capabilities: IndexerCapabilities {
                rss: false,
                supported_ids: HashMap::new(),
                supported_query_facets: vec![
                    "series".to_string(),
                    "movie".to_string(),
                    "anime".to_string(),
                ],
                search: true,
                // The host offers text search only to indexers that name a
                // query parameter; `search.list` takes the query as `q`.
                query_param: Some("q".to_string()),
                protocols: vec![IndexerProtocol::Http],
                feed_modes: vec![IndexerFeedMode::InteractiveSearch],
                search_inputs: vec![
                    IndexerSearchInput::TextQuery,
                    IndexerSearchInput::TitleQuery,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec![],
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(MAX_RESULTS as u32),
                    max_page_size: Some(MAX_RESULTS as u32),
                    rate_limit_hint_seconds: Some(RATE_LIMIT_SECONDS),
                    paged_search: true,
                    ..IndexerLimitCapabilities::default()
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
                http: Some(IndexerHttpCapabilities {
                    transports: vec![HttpTransport::ProviderPage],
                    providers: vec![release::PROVIDER.to_string()],
                }),
                ..IndexerCapabilities::default()
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec![API_HOST.to_string()],
            rate_limit_seconds: Some(RATE_LIMIT_SECONDS as i64),
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
            "Your own Google API key with the YouTube Data API v3 enabled. Stored encrypted; sent only to Google, in a request header.",
        ),
        ConfigFieldDef {
            options: ["moderate", "none", "strict"]
                .into_iter()
                .map(|value| ConfigFieldOption {
                    value: value.to_string(),
                    label: match value {
                        "moderate" => "Moderate".to_string(),
                        "none" => "None".to_string(),
                        _ => "Strict".to_string(),
                    },
                    config_overrides: BTreeMap::new(),
                })
                .collect(),
            ..field(
                "safe_search",
                "Safe Search",
                ConfigFieldType::Select,
                false,
                Some("moderate"),
                "YouTube's restricted-content filter for search results.",
            )
        },
        field(
            "channel_ids",
            "Channel Allow-List",
            ConfigFieldType::Tag,
            false,
            None,
            "Optional channel ids (UC…). When set, every search is limited to these channels, one search call per channel, and season or episode lookups read the channels' uploads instead of searching. At most 10.",
        ),
        ConfigFieldDef {
            advanced: true,
            ..field(
                "region_code",
                "Region",
                ConfigFieldType::String,
                false,
                None,
                "Optional ISO 3166-1 alpha-2 country code, for example US. Results are searched for this region and region-blocked videos are marked geo-restricted.",
            )
        },
        ConfigFieldDef {
            advanced: true,
            ..field(
                "relevance_language",
                "Relevance Language",
                ConfigFieldType::String,
                false,
                None,
                "Optional ISO 639-1 language code, for example en. Results most relevant to this language rank first.",
            )
        },
    ]
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
    help_text: &str,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: ConfigFieldValueSource::User,
        help_text: Some(help_text.to_string()),
        ..ConfigFieldDef::default()
    }
}

fn config_value(key: &str) -> Option<String> {
    config::get(key).ok().flatten()
}

/// The host transport. The key rides in a header so it never appears in a
/// URL the host or an intermediary may log.
struct HostFetch<'a> {
    api_key: &'a str,
}

impl Fetch for HostFetch<'_> {
    async fn get(&self, call: &ApiCall) -> Result<HttpReply, YoutubeError> {
        let response = component::http(PluginHttpRequest {
            url: call.url(),
            method: Some("GET".to_string()),
            headers: BTreeMap::from([
                ("Accept".to_string(), "application/json".to_string()),
                ("User-Agent".to_string(), USER_AGENT.to_string()),
                (API_KEY_HEADER.to_string(), self.api_key.to_string()),
            ]),
            body: Vec::new(),
        })
        .await
        .map_err(|error| match error {
            HostError::Cancelled => YoutubeError::Cancelled,
            other => YoutubeError::Transport(other.to_string()),
        })?;
        Ok(HttpReply {
            status: response.status,
            body: response.body,
        })
    }
}

async fn search(request: SearchRequest) -> FnResult<SearchResponse> {
    let config = YoutubeConfig::from_values(config_value)
        .map_err(|error| structured_plugin_error(plugin_error(&error, None)))?;

    let cache_key = cache::request_key(&config, &request);
    if let Some(response) = cache::lookup(&cache_key) {
        return Ok(response);
    }

    let fetch = HostFetch {
        api_key: &config.api_key,
    };
    let outcome = engine::run_page(&config, &request, &fetch).await;
    match outcome {
        Ok(PageOutcome {
            response,
            ceiling_reached,
            warnings,
        }) => {
            log_warnings(&config, &warnings);
            if ceiling_reached {
                return Err(structured_plugin_error(partial_error(
                    response,
                    IndexerSearchIncompleteReason::PageCeilingReached,
                    "search read to this indexer's page ceiling".to_string(),
                )));
            }
            cache::store(&cache_key, &response);
            Ok(response)
        }
        Err(Failure {
            error,
            partial,
            warnings,
        }) => {
            log_warnings(&config, &warnings);
            log(
                LogLevel::Warn,
                &redact(&format!("youtube search failed: {error}"), &config.api_key),
            );
            Err(structured_plugin_error(plugin_error(&error, partial)))
        }
    }
}

fn log_warnings(config: &YoutubeConfig, warnings: &[String]) {
    for warning in warnings {
        log(LogLevel::Warn, &redact(warning, &config.api_key));
    }
}

/// Belt and braces: no message this plugin builds contains the key, but any
/// that ever did would leave without it.
pub(crate) fn redact(message: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        message.to_string()
    } else {
        message.replace(api_key, "[redacted]")
    }
}

fn partial_error(
    response: SearchResponse,
    reason: IndexerSearchIncompleteReason,
    detail: String,
) -> PluginError {
    let code = if reason == IndexerSearchIncompleteReason::RateLimited {
        PluginErrorCode::RateLimited
    } else {
        PluginErrorCode::UpstreamUnavailable
    };
    PluginError {
        code,
        public_message: "YouTube search returned part of a page".to_string(),
        debug_message: Some(detail),
        retry_after_seconds: None,
        details: Some(PluginErrorDetails::IndexerSearch(
            IndexerSearchPluginError::PartialResults {
                response: Box::new(response),
                reason,
                retry_after_seconds: None,
            },
        )),
    }
}

/// The SDK error for a failure. `partial` holds results read before it.
pub(crate) fn plugin_error(error: &YoutubeError, partial: Option<SearchResponse>) -> PluginError {
    let message = error.to_string();
    if let Some(response) = partial.filter(|response| !response.results.is_empty()) {
        let reason = match error {
            YoutubeError::QuotaExceeded | YoutubeError::RateLimited => {
                IndexerSearchIncompleteReason::RateLimited
            }
            YoutubeError::Malformed(_) => IndexerSearchIncompleteReason::MalformedContent,
            _ => IndexerSearchIncompleteReason::UpstreamFailure,
        };
        return partial_error(response, reason, message);
    }
    let (code, details) = match error {
        YoutubeError::InvalidKey => (PluginErrorCode::AuthFailed, None),
        YoutubeError::ApiDisabled | YoutubeError::InvalidConfig(_) => {
            (PluginErrorCode::InvalidConfig, None)
        }
        YoutubeError::BadRequest { .. } => (PluginErrorCode::InvalidConfig, None),
        YoutubeError::QuotaExceeded | YoutubeError::RateLimited => (
            PluginErrorCode::RateLimited,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::RateLimited,
                    retry_after_seconds: None,
                },
            )),
        ),
        YoutubeError::NotFound { .. } | YoutubeError::InvalidCursor => {
            (PluginErrorCode::Permanent, None)
        }
        YoutubeError::Upstream { .. } | YoutubeError::Transport(_) => {
            (PluginErrorCode::UpstreamUnavailable, None)
        }
        YoutubeError::Cancelled | YoutubeError::Malformed(_) => (PluginErrorCode::Temporary, None),
    };
    PluginError {
        code,
        public_message: message.clone(),
        debug_message: Some(message),
        retry_after_seconds: None,
        details,
    }
}

scryer_indexer_component_main!(descriptor = build_descriptor, search = search,);
