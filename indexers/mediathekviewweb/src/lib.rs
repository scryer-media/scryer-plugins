//! MediathekViewWeb indexer: searches the public MediathekViewWeb film list
//! of the German-language public broadcasters and returns HTTP-family
//! releases whose resources are the broadcasters' own MP4 files, subtitle
//! sidecars and programme pages. The native HTTP client takes the files; a
//! client that declares `provider_page` takes the page.

mod api;
mod cursor;
#[cfg(test)]
mod embed;
mod engine;
mod release;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use scryer_plugin_pdk::component::{self, HostError, structured_plugin_error};
use scryer_plugin_pdk::log::{LogLevel, log};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, HttpTransport, IndexerCapabilities,
    IndexerDescriptor, IndexerFeedMode, IndexerHttpCapabilities, IndexerLimitCapabilities,
    IndexerProtocol, IndexerResponseFeatures, IndexerSearchIncompleteReason, IndexerSearchInput,
    IndexerSearchPluginError, IndexerSourceKind, IndexerStrategyPlanCapability, PluginDescriptor,
    PluginError, PluginErrorCode, PluginErrorDetails, PluginSearchRequest as SearchRequest,
    PluginSearchResponse as SearchResponse, ProviderDescriptor, SDK_VERSION,
    current_sdk_constraint,
};

use crate::api::{API_HOST, API_URL, MAX_RESULTS, MvwConfig, MvwError, QueryCall};
use crate::engine::{Fetch, HttpReply, PageOutcome};

const PLUGIN_ID: &str = "mediathekviewweb";
const USER_AGENT: &str = "Scryer MediathekViewWeb Indexer/0.1";
/// Seconds between calls the host should keep for this indexer.
const RATE_LIMIT_SECONDS: u32 = 1;

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "MediathekViewWeb".to_string(),
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
            // Strategies run one at a time so a plan never bursts a free
            // community service.
            strategy_plan: Some(IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 1,
            }),
            source_kind: IndexerSourceKind::Http,
            capabilities: IndexerCapabilities {
                rss: false,
                supported_ids: HashMap::new(),
                supported_query_facets: vec!["series".to_string(), "movie".to_string()],
                search: true,
                // The host offers text search only to indexers that name a
                // query parameter.
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
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
                http: Some(IndexerHttpCapabilities {
                    transports: vec![
                        HttpTransport::DirectFile,
                        HttpTransport::Hls,
                        HttpTransport::ProviderPage,
                    ],
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
            "channels",
            "Channel Allow-List",
            ConfigFieldType::Tag,
            None,
            "Optional channel names as MediathekViewWeb spells them, for example ARD, ZDF, KiKA or ARTE.DE. When set, only results from these channels are returned. At most 20.",
        ),
        field(
            "include_audio_description",
            "Include Audio Description",
            ConfigFieldType::Bool,
            Some("false"),
            "Also return audio-described versions (Audiodeskription, Hörfassung). They are separate releases with the audio-description role set.",
        ),
        field(
            "include_sign_language",
            "Include Sign Language",
            ConfigFieldType::Bool,
            Some("false"),
            "Also return versions with sign-language interpretation (Gebärdensprache, DGS). They are separate releases.",
        ),
        field(
            "min_duration_minutes",
            "Minimum Duration (minutes)",
            ConfigFieldType::Number,
            None,
            "Optional. Results shorter than this are dropped, which removes clips and trailers. Results with no duration are kept.",
        ),
    ]
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    default_value: Option<&str>,
    help_text: &str,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required: false,
        default_value: default_value.map(str::to_string),
        value_source: ConfigFieldValueSource::User,
        help_text: Some(help_text.to_string()),
        ..ConfigFieldDef::default()
    }
}

fn config_value(key: &str) -> Option<String> {
    config::get(key).ok().flatten()
}

/// The host transport.
struct HostFetch;

impl Fetch for HostFetch {
    async fn post(&self, call: &QueryCall) -> Result<HttpReply, MvwError> {
        let response = component::http(PluginHttpRequest {
            url: API_URL.to_string(),
            method: Some("POST".to_string()),
            headers: BTreeMap::from([
                ("Accept".to_string(), "application/json".to_string()),
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("User-Agent".to_string(), USER_AGENT.to_string()),
            ]),
            body: call.body().into_bytes(),
        })
        .await
        .map_err(|error| match error {
            HostError::Cancelled => MvwError::Cancelled,
            other => MvwError::Transport(other.to_string()),
        })?;
        Ok(HttpReply {
            status: response.status,
            body: response.body,
        })
    }
}

async fn search(request: SearchRequest) -> FnResult<SearchResponse> {
    let config = MvwConfig::from_values(config_value)
        .map_err(|error| structured_plugin_error(plugin_error(&error)))?;

    match engine::run_page(&config, &request, &HostFetch).await {
        Ok(PageOutcome {
            response,
            ceiling_reached: true,
        }) => Err(structured_plugin_error(ceiling_error(response))),
        Ok(PageOutcome { response, .. }) => Ok(response),
        Err(error) => {
            log(
                LogLevel::Warn,
                &format!("mediathekviewweb search failed: {error}"),
            );
            Err(structured_plugin_error(plugin_error(&error)))
        }
    }
}

/// The last page a search may read, reported as partial so the host keeps
/// its results and stops asking for more.
pub(crate) fn ceiling_error(response: SearchResponse) -> PluginError {
    PluginError {
        code: PluginErrorCode::UpstreamUnavailable,
        public_message: "MediathekViewWeb search returned part of its results".to_string(),
        debug_message: Some("search read to this indexer's page ceiling".to_string()),
        retry_after_seconds: None,
        details: Some(PluginErrorDetails::IndexerSearch(
            IndexerSearchPluginError::PartialResults {
                response: Box::new(response),
                reason: IndexerSearchIncompleteReason::PageCeilingReached,
                retry_after_seconds: None,
            },
        )),
    }
}

/// The SDK error for a failure.
pub(crate) fn plugin_error(error: &MvwError) -> PluginError {
    let message = error.to_string();
    let (code, details) = match error {
        MvwError::InvalidConfig(_) => (PluginErrorCode::InvalidConfig, None),
        MvwError::RateLimited => (
            PluginErrorCode::RateLimited,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::RateLimited,
                    retry_after_seconds: None,
                },
            )),
        ),
        MvwError::Upstream { .. } | MvwError::Transport(_) => {
            (PluginErrorCode::UpstreamUnavailable, None)
        }
        MvwError::Cancelled => (PluginErrorCode::Temporary, None),
        MvwError::InvalidCursor
        | MvwError::BadStatus { .. }
        | MvwError::Rejected(_)
        | MvwError::Malformed(_) => (PluginErrorCode::Permanent, None),
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
