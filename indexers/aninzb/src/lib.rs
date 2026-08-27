use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use newznab_common::{
    Capabilities, IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor,
    IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures,
    IndexerSearchInput, IndexerSourceKind, NewznabHitBudget, NewznabHttpBehavior, PluginDescriptor,
    PluginSearchSubjectKind, ProviderDescriptor, SDK_VERSION, SearchRequest, SearchResponse,
    SearchResult, current_sdk_constraint, hit_budget_retry_after_seconds, reserve_hit_budget_uses,
};
use scryer_plugin_pdk::*;
use scryer_plugin_pdk::component::{self, LogLevel, StartRateGate, StreamExt, structured_plugin_error};
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, ConfigFieldValueSource,
    IndexerSearchIncompleteReason, IndexerSearchInvalidResponseKind, IndexerSearchPluginError,
    PluginError, PluginErrorCode, PluginErrorDetails,
};
use serde::Deserialize;
use url::Url;

macro_rules! log {
    ($level:expr, $($argument:tt)*) => {
        component::log($level, format!($($argument)*))
    };
}

const ANINZB_API_BASE_URL: &str = "https://api.aninzb.moe/";
const ANINZB_API_HOST: &str = "api.aninzb.moe";
const LEGACY_BASE_URL_DEFAULT: &str = "https://aninzb.moe";
const API_MAX_RESULTS: usize = 50;
const MAX_PARTITION_REQUESTS: usize = 64;
const MAX_SIZE_PARTITION_DEPTH: u8 = 12;
const MAX_API_RESPONSE_BYTES: usize = 20 * 1024 * 1024;
const API_REQUESTS_PER_SECOND: u32 = 3;
const DEFAULT_HOURLY_HIT_CAP: u32 = 1000;
const DEFAULT_DAILY_HIT_CAP: u32 = 5000;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[derive(Clone, Debug, Default)]
struct LegacyAniNzbConfig {
    base_url: Option<String>,
    api_key_present: bool,
    api_path: Option<String>,
    additional_params: Option<String>,
    hourly_hit_cap: Option<String>,
    daily_hit_cap: Option<String>,
}

impl LegacyAniNzbConfig {
    fn from_host() -> Self {
        Self {
            base_url: config_optional("base_url"),
            api_key_present: component::config_get("api_key")
                .is_some_and(|value| !value.trim().is_empty()),
            api_path: config_optional("api_path"),
            additional_params: config_optional("additional_params"),
            hourly_hit_cap: config_optional("hourly_hit_cap"),
            daily_hit_cap: config_optional("daily_hit_cap"),
        }
    }

    fn is_present(&self) -> bool {
        self.base_url.is_some()
            || self.api_key_present
            || self.api_path.is_some()
            || self.additional_params.is_some()
            || self.hourly_hit_cap.is_some()
            || self.daily_hit_cap.is_some()
    }
}

#[derive(Clone, Debug)]
struct AniNzbConfig {
    api_base_url: &'static str,
    http_behavior: NewznabHttpBehavior,
}

impl AniNzbConfig {
    fn from_host() -> Self {
        let legacy = LegacyAniNzbConfig::from_host();
        if legacy.is_present() {
            log!(
                LogLevel::Debug,
                "AniNZB legacy configuration ignored; using fixed public API"
            );
        }
        migrate_legacy_config(legacy)
    }
}

fn migrate_legacy_config(_legacy: LegacyAniNzbConfig) -> AniNzbConfig {
    AniNzbConfig {
        api_base_url: ANINZB_API_BASE_URL,
        http_behavior: NewznabHttpBehavior {
            plugin_id: "aninzb".to_string(),
            user_agent: USER_AGENT.to_string(),
            pre_request_delay: Duration::ZERO,
            retry_total_budget: Duration::from_secs(300),
            retry_default_delay: Duration::from_secs(60),
            retry_max_delay: Duration::from_secs(300),
            retry_max_attempts: 5,
            max_search_pages: 1,
            hit_budget: Some(NewznabHitBudget {
                var_key: "aninzb.http_hits".to_string(),
                hourly_limit: DEFAULT_HOURLY_HIT_CAP,
                daily_limit: DEFAULT_DAILY_HIT_CAP,
            }),
        },
    }
}

#[derive(Debug, Deserialize)]
struct AniNzbApiResponse {
    #[serde(default, rename = "total_count")]
    total_count: Option<u64>,
    #[serde(default)]
    items: Option<Vec<AniNzbApiItem>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AniNzbApiItem {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    anidb: Option<u64>,
    #[serde(default)]
    series_name: Option<Vec<String>>,
    #[serde(default)]
    episode: Option<f64>,
    #[serde(default)]
    season: Option<i64>,
    #[serde(default)]
    tvdb: Option<String>,
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    date: Option<i64>,
    #[serde(default)]
    nzb: Option<String>,
    #[serde(default)]
    poster: Option<String>,
    #[serde(default)]
    subtitles: Option<Vec<String>>,
    #[serde(default)]
    screenshots: Option<Vec<String>>,
    #[serde(default)]
    thumbnails: Option<Vec<String>>,
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "aninzb".to_string(),
        name: "AniNZB Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "aninzb".to_string(),
            provider_aliases: vec![],
            search_semantics_version: Some(1),
            source_kind: IndexerSourceKind::Usenet,
            capabilities: Capabilities {
                supported_ids: HashMap::from([
                    ("series".into(), vec!["tvdb_id".into(), "anidb_id".into()]),
                    ("anime".into(), vec!["tvdb_id".into(), "anidb_id".into()]),
                ]),
                deduplicates_aliases: false,
                season_param: Some("season".into()),
                episode_param: Some("episode".into()),
                query_param: Some("name".into()),
                supported_query_facets: vec![],
                search: true,
                imdb_search: false,
                tvdb_search: true,
                anidb_search: true,
                rss: true,
                protocols: vec![IndexerProtocol::Usenet],
                feed_modes: vec![
                    IndexerFeedMode::Recent,
                    IndexerFeedMode::Rss,
                    IndexerFeedMode::AutomaticSearch,
                    IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![
                    IndexerSearchInput::TitleQuery,
                    IndexerSearchInput::IdQuery,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec!["tvdb_id".into(), "anidb_id".into()],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(API_MAX_RESULTS as u32),
                    max_page_size: Some(API_MAX_RESULTS as u32),
                    max_pages: Some(1),
                    rate_limit_hint_seconds: None,
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: None,
                response_features: Some(IndexerResponseFeatures {
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: legacy_config_fields(),
            allowed_hosts: vec![ANINZB_API_HOST.to_string()],
            rate_limit_seconds: None,
        }),
    }
}

fn legacy_config_fields() -> Vec<ConfigFieldDef> {
    vec![ConfigFieldDef {
        key: "base_url".to_string(),
        label: "Base URL".to_string(),
        field_type: ConfigFieldType::String,
        required: true,
        default_value: Some(LEGACY_BASE_URL_DEFAULT.to_string()),
        value_source: ConfigFieldValueSource::User,
        role: Some(ConfigFieldRole::ConnectionUrl),
        host_binding: None,
        options: vec![],
        help_text: Some(
            "Retained for compatibility; AniNZB always uses its fixed public API endpoint."
                .to_string(),
        ),
    }]
}

async fn search(request: SearchRequest) -> Result<SearchResponse, Error> {
    if request_is_movie_shaped(&request) {
        return Ok(SearchResponse::default());
    }

    let config = AniNzbConfig::from_host();
    execute_api_search(&config, &request).await
}

async fn execute_api_search(
    config: &AniNzbConfig,
    request: &SearchRequest,
) -> Result<SearchResponse, Error> {
    let initial_queries = initial_api_queries(request);
    let mut queries = Vec::new();
    let mut seen_queries = HashSet::new();
    let mut seen_results = HashSet::new();
    let mut results = Vec::new();
    let mut incomplete_reason = None;
    let mut invalid_kind = None;
    let mut retry_after_seconds = None;
    let mut completed_request = false;
    let mut request_count = 0;
    let start_gate = StartRateGate::new(
        "aninzb.api.start_rate",
        API_REQUESTS_PER_SECOND,
        1_000,
    );

    let mut probe_batch = Vec::with_capacity(initial_queries.len() * 2);
    for query in initial_queries {
        probe_batch.push((
            query.clone(),
            build_api_size_probe_url(config, &query, "asc")?,
        ));
        probe_batch.push((
            query.clone(),
            build_api_size_probe_url(config, &query, "desc")?,
        ));
    }
    let probe_len = u32::try_from(probe_batch.len()).unwrap_or(u32::MAX);
    if probe_batch.len() > MAX_PARTITION_REQUESTS {
        return Err(incomplete_search_error(
            SearchResponse::default(),
            IndexerSearchIncompleteReason::SaturatedPartition,
            None,
            false,
            None,
        ));
    }
    if let Err(error) = reserve_hit_budget_uses(&config.http_behavior, probe_len) {
        if newznab_common::is_hit_budget_exhausted_error(&error) {
            return Err(incomplete_search_error(
                SearchResponse::default(),
                IndexerSearchIncompleteReason::RateLimited,
                hit_budget_retry_after_seconds(&config.http_behavior, probe_len)?,
                false,
                None,
            ));
        }
        return Err(error);
    }
    request_count += probe_batch.len();
    let probe_pages = execute_api_search_batch(&probe_batch, &start_gate).await?;
    let mut probes = probe_batch.into_iter().zip(probe_pages);
    while let Some(((mut query, _), ascending)) = probes.next() {
        let Some(((_, _), descending)) = probes.next() else {
            incomplete_reason = Some(IndexerSearchIncompleteReason::FanoutBranchFailed);
            break;
        };
        match (ascending, descending) {
            (Ok(ascending), Ok(descending)) => {
                completed_request = true;
                append_page_results(&ascending, &mut seen_results, &mut results);
                append_page_results(&descending, &mut seen_results, &mut results);
                if ascending.is_saturated() || descending.is_saturated() {
                    if let Some((minimum, maximum)) = probe_size_bounds(&ascending, &descending) {
                        query.min_size = Some(minimum);
                        query.max_size = Some(maximum);
                        queries.push(query);
                    } else {
                        incomplete_reason = Some(IndexerSearchIncompleteReason::SaturatedPartition);
                    }
                }
            }
            (Ok(page), Err(failure)) | (Err(failure), Ok(page)) => {
                completed_request = true;
                append_page_results(&page, &mut seen_results, &mut results);
                record_api_failure(
                    &failure,
                    &mut incomplete_reason,
                    &mut invalid_kind,
                    &mut retry_after_seconds,
                );
                if page.is_saturated() {
                    queries.push(query);
                }
            }
            (Err(ascending), Err(descending)) => {
                record_api_failure(
                    &ascending,
                    &mut incomplete_reason,
                    &mut invalid_kind,
                    &mut retry_after_seconds,
                );
                record_api_failure(
                    &descending,
                    &mut incomplete_reason,
                    &mut invalid_kind,
                    &mut retry_after_seconds,
                );
            }
        }
    }

    while !queries.is_empty() {
        let remaining = MAX_PARTITION_REQUESTS.saturating_sub(request_count);
        if remaining == 0 {
            incomplete_reason = Some(IndexerSearchIncompleteReason::SaturatedPartition);
            break;
        }

        let mut batch = Vec::new();
        while batch.len() < remaining {
            let Some(query) = queries.pop() else {
                break;
            };
            let url = build_api_search_url(config, &query)?;
            if seen_queries.insert(url.clone()) {
                batch.push((query, url));
            }
        }
        if batch.is_empty() {
            continue;
        }

        let batch_len = u32::try_from(batch.len()).unwrap_or(u32::MAX);
        if let Err(error) = reserve_hit_budget_uses(&config.http_behavior, batch_len) {
            if newznab_common::is_hit_budget_exhausted_error(&error) {
                incomplete_reason = Some(IndexerSearchIncompleteReason::RateLimited);
                retry_after_seconds =
                    hit_budget_retry_after_seconds(&config.http_behavior, batch_len)?;
                break;
            }
            return Err(error);
        }
        request_count += batch.len();

        let pages = execute_api_search_batch(&batch, &start_gate).await?;
        for ((query, _), page) in batch.into_iter().zip(pages) {
            match page {
                Ok(page) => {
                    completed_request = true;
                    append_page_results(&page, &mut seen_results, &mut results);

                    if page.is_saturated() {
                        if let Some((lower, upper)) = query.size_partitions(&page.items) {
                            queries.push(upper);
                            queries.push(lower);
                        } else {
                            incomplete_reason =
                                Some(IndexerSearchIncompleteReason::SaturatedPartition);
                        }
                    }
                }
                Err(failure) => {
                    record_api_failure(
                        &failure,
                        &mut incomplete_reason,
                        &mut invalid_kind,
                        &mut retry_after_seconds,
                    );
                }
            }
        }
    }

    let response = SearchResponse {
        results,
        ..SearchResponse::default()
    };
    if let Some(reason) = incomplete_reason {
        return Err(incomplete_search_error(
            response,
            reason,
            retry_after_seconds,
            completed_request,
            invalid_kind,
        ));
    }

    Ok(response)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ApiSearchBranch {
    AniDb(String),
    Tvdb(String),
    Name(String),
    Filename(String),
    ScopedFilename { name: String, token: String },
    Recent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ApiSearchQuery {
    branch: ApiSearchBranch,
    season: Option<u32>,
    episode: Option<u32>,
    min_size: Option<i64>,
    max_size: Option<i64>,
    partition_depth: u8,
}

impl ApiSearchQuery {
    fn size_partitions(&self, items: &[AniNzbApiItem]) -> Option<(Self, Self)> {
        if self.partition_depth >= MAX_SIZE_PARTITION_DEPTH {
            return None;
        }

        let mut sizes: Vec<i64> = items.iter().filter_map(|item| item.size).collect();
        sizes.sort_unstable();
        let pivot = *sizes.get(sizes.len() / 2)?;
        let lower_bound = self.min_size.unwrap_or(0);
        let upper_bound = self.max_size.unwrap_or(i64::MAX);
        if pivot < lower_bound || pivot >= upper_bound {
            return None;
        }

        let mut lower = self.clone();
        lower.max_size = Some(pivot);
        lower.partition_depth += 1;

        let mut upper = self.clone();
        upper.min_size = pivot.checked_add(1);
        upper.partition_depth += 1;
        upper.min_size.map(|_| (lower, upper))
    }
}

struct ApiSearchPage {
    total_count: Option<u64>,
    items: Vec<AniNzbApiItem>,
}

impl ApiSearchPage {
    fn is_saturated(&self) -> bool {
        self.total_count
            .map_or(self.items.len() >= API_MAX_RESULTS, |total_count| {
                total_count > self.items.len() as u64
            })
    }
}

fn append_page_results(
    page: &ApiSearchPage,
    seen_results: &mut HashSet<String>,
    results: &mut Vec<SearchResult>,
) {
    for item in &page.items {
        let Some(result) = api_item_to_search_result(item) else {
            continue;
        };
        if seen_results.insert(result_identity(&result)) {
            results.push(result);
        }
    }
}

fn probe_size_bounds(ascending: &ApiSearchPage, descending: &ApiSearchPage) -> Option<(i64, i64)> {
    let minimum = ascending.items.iter().filter_map(|item| item.size).min()?;
    let maximum = descending.items.iter().filter_map(|item| item.size).max()?;
    (minimum <= maximum).then_some((minimum, maximum))
}

fn initial_api_queries(request: &SearchRequest) -> Vec<ApiSearchQuery> {
    let anime_episode = request_is_anime_shaped(request)
        .then(|| request.absolute_episode.or(request.episode))
        .flatten()
        .or(request.episode)
        .or(request.absolute_episode);
    let tvdb_episode = request.episode.or(request.absolute_episode);
    let season_pack_search =
        request.season.is_some() && request.episode.is_none() && request.absolute_episode.is_none();
    let mut queries = Vec::new();

    if let Some(anidb_id) = request_id(request, "anidb_id") {
        queries.push(ApiSearchQuery {
            branch: ApiSearchBranch::AniDb(anidb_id),
            // AniDB IDs identify a season. Supplying a second season filter can
            // make AniNZB miss otherwise matching releases.
            season: None,
            episode: anime_episode,
            min_size: None,
            max_size: None,
            partition_depth: 0,
        });
    }
    if let Some(tvdb_id) = request_id(request, "tvdb_id") {
        queries.push(ApiSearchQuery {
            branch: ApiSearchBranch::Tvdb(tvdb_id),
            season: request.season,
            episode: tvdb_episode,
            min_size: None,
            max_size: None,
            partition_depth: 0,
        });
    }
    if let Some(name) = search_name(request) {
        queries.push(ApiSearchQuery {
            branch: ApiSearchBranch::Name(name.clone()),
            season: request.season,
            episode: anime_episode,
            min_size: None,
            max_size: None,
            partition_depth: 0,
        });
        queries.push(ApiSearchQuery {
            branch: if season_pack_search {
                ApiSearchBranch::ScopedFilename {
                    name,
                    token: format!("S{:02}", request.season.expect("season pack search")),
                }
            } else {
                ApiSearchBranch::Filename(name)
            },
            // Season-pack filename searches deliberately omit the structured
            // season filter because AniNZB pack rows can report a null season.
            season: (!season_pack_search).then_some(request.season).flatten(),
            episode: anime_episode,
            min_size: None,
            max_size: None,
            partition_depth: 0,
        });
    }
    if queries.is_empty() {
        queries.push(ApiSearchQuery {
            branch: ApiSearchBranch::Recent,
            season: request.season,
            episode: anime_episode,
            min_size: None,
            max_size: None,
            partition_depth: 0,
        });
    }
    queries
}

#[derive(Clone, Debug)]
enum ApiRequestFailure {
    RateLimited(Option<i64>),
    Invalid(IndexerSearchInvalidResponseKind, String),
    Upstream(String),
}

impl ApiRequestFailure {
    fn incomplete_reason(&self) -> IndexerSearchIncompleteReason {
        match self {
            Self::RateLimited(_) => IndexerSearchIncompleteReason::RateLimited,
            Self::Invalid(_, _) => IndexerSearchIncompleteReason::MalformedContent,
            Self::Upstream(_) => IndexerSearchIncompleteReason::FanoutBranchFailed,
        }
    }

    fn invalid_kind(&self) -> Option<IndexerSearchInvalidResponseKind> {
        match self {
            Self::Invalid(kind, _) => Some(*kind),
            _ => None,
        }
    }

    fn retry_after_seconds(&self) -> Option<i64> {
        match self {
            Self::RateLimited(retry_after_seconds) => *retry_after_seconds,
            _ => None,
        }
    }
}

fn record_api_failure(
    failure: &ApiRequestFailure,
    incomplete_reason: &mut Option<IndexerSearchIncompleteReason>,
    invalid_kind: &mut Option<IndexerSearchInvalidResponseKind>,
    retry_after_seconds: &mut Option<i64>,
) {
    match failure {
        ApiRequestFailure::RateLimited(_) => {
            log!(LogLevel::Warn, "AniNZB search branch was rate limited");
        }
        ApiRequestFailure::Invalid(_, message) | ApiRequestFailure::Upstream(message) => {
            log!(LogLevel::Warn, "AniNZB search branch failed: {}", message);
        }
    }
    *incomplete_reason = Some(failure.incomplete_reason());
    if invalid_kind.is_none() {
        *invalid_kind = failure.invalid_kind();
    }
    if retry_after_seconds.is_none() {
        *retry_after_seconds = failure.retry_after_seconds();
    }
}

async fn execute_api_search_batch(
    batch: &[(ApiSearchQuery, String)],
    start_gate: &StartRateGate,
) -> Result<Vec<Result<ApiSearchPage, ApiRequestFailure>>, Error> {
    let mut ordered = component::stream::iter(batch.iter().enumerate().map(|(index, (_, url))| {
        let start_gate = start_gate.clone();
        let url = url.clone();
        async move {
            let response = match start_gate.acquire().await {
                Ok(()) => component::http(PluginHttpRequest {
                    url,
                    method: Some("GET".to_string()),
                    headers: BTreeMap::from([
                        ("Accept".to_string(), "application/json".to_string()),
                        ("User-Agent".to_string(), USER_AGENT.to_string()),
                    ]),
                    body: Vec::new(),
                })
                .await
                .map_err(|error| {
                    ApiRequestFailure::Upstream(format!("AniNZB API request failed: {error}"))
                })
                .and_then(parse_api_http_response),
                Err(wait) => Err(ApiRequestFailure::RateLimited(Some(
                    i64::try_from(wait.retry_after_ms.div_ceil(1_000)).unwrap_or(i64::MAX),
                ))),
            };
            (index, response)
        }
    }))
    .buffer_unordered(16)
    .collect::<Vec<_>>()
    .await;
    ordered.sort_by_key(|(index, _)| *index);
    Ok(ordered.into_iter().map(|(_, response)| response).collect())
}

fn parse_api_http_response(
    response: PluginHttpResponse,
) -> Result<ApiSearchPage, ApiRequestFailure> {
    if response.status == 429 {
        return Err(ApiRequestFailure::RateLimited(
            response_header(&response.headers, "retry-after")
                .and_then(|value| value.parse::<i64>().ok()),
        ));
    }
    if !(200..300).contains(&response.status) {
        return Err(ApiRequestFailure::Upstream(format!(
            "AniNZB API returned HTTP {}",
            response.status
        )));
    }

    let content_type = response_header(&response.headers, "content-type").unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().ends_with("/json") || value.trim().ends_with("+json"))
    {
        return Err(ApiRequestFailure::Invalid(
            IndexerSearchInvalidResponseKind::UnexpectedContentType,
            "AniNZB API returned a non-JSON content type".to_string(),
        ));
    }
    validate_api_response_size(response.body.len()).map_err(|error| {
        ApiRequestFailure::Invalid(
            IndexerSearchInvalidResponseKind::TruncatedBody,
            error.to_string(),
        )
    })?;
    let body = std::str::from_utf8(&response.body).map_err(|error| {
        ApiRequestFailure::Invalid(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("AniNZB API response was not valid UTF-8: {error}"),
        )
    })?;
    let parsed = parse_api_response(body).map_err(|error| {
        ApiRequestFailure::Invalid(
            IndexerSearchInvalidResponseKind::MalformedBody,
            error.to_string(),
        )
    })?;
    if parsed.total_count.is_none() && parsed.items.is_none() {
        return Err(ApiRequestFailure::Invalid(
            IndexerSearchInvalidResponseKind::InvalidRoot,
            "AniNZB API response did not contain a result root".to_string(),
        ));
    }

    Ok(ApiSearchPage {
        total_count: parsed.total_count,
        items: parsed.items.unwrap_or_default(),
    })
}

fn response_header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn incomplete_search_error(
    response: SearchResponse,
    reason: IndexerSearchIncompleteReason,
    retry_after_seconds: Option<i64>,
    completed_request: bool,
    invalid_kind: Option<IndexerSearchInvalidResponseKind>,
) -> Error {
    let code = if reason == IndexerSearchIncompleteReason::RateLimited {
        PluginErrorCode::RateLimited
    } else {
        PluginErrorCode::UpstreamUnavailable
    };
    let details = if !completed_request {
        if let Some(kind) = invalid_kind {
            IndexerSearchPluginError::InvalidResponse { kind }
        } else {
            IndexerSearchPluginError::Deferred {
                reason,
                retry_after_seconds,
            }
        }
    } else {
        IndexerSearchPluginError::PartialResults {
            response: Box::new(response),
            reason,
            retry_after_seconds,
        }
    };
    structured_plugin_error(PluginError {
        code,
        public_message: "AniNZB search did not complete".to_string(),
        debug_message: Some("one or more AniNZB search branches did not complete".to_string()),
        retry_after_seconds,
        details: Some(PluginErrorDetails::IndexerSearch(details)),
    })
}

fn parse_api_response(body: &str) -> Result<AniNzbApiResponse, Error> {
    validate_api_response_size(body.len())?;
    serde_json::from_str(body)
        .map_err(|error| Error::msg(format!("invalid AniNZB API response: {error}")))
}

fn validate_api_response_size(response_bytes: usize) -> Result<(), Error> {
    if response_bytes > MAX_API_RESPONSE_BYTES {
        return Err(Error::msg(format!(
            "AniNZB API response exceeded {MAX_API_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn build_api_search_url(config: &AniNzbConfig, request: &ApiSearchQuery) -> Result<String, Error> {
    let mut url = Url::parse(config.api_base_url)
        .map_err(|error| Error::msg(format!("invalid fixed AniNZB API URL: {error}")))?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);

    {
        let mut query = url.query_pairs_mut();
        match &request.branch {
            ApiSearchBranch::AniDb(anidb_id) => query.append_pair("anidb", anidb_id),
            ApiSearchBranch::Tvdb(tvdb_id) => query.append_pair("tvdb", tvdb_id),
            ApiSearchBranch::Name(name) => query.append_pair("name", name),
            ApiSearchBranch::Filename(filename) => query.append_pair("filename", filename),
            ApiSearchBranch::ScopedFilename { name, token } => {
                query.append_pair("name", name);
                query.append_pair("filename", token)
            }
            ApiSearchBranch::Recent => query.append_pair("source", "release"),
        };
        if let Some(season) = request.season {
            query.append_pair("season", &season.to_string());
        }
        if let Some(episode) = request.episode {
            query.append_pair("episode", &episode.to_string());
        }
        if let Some(min_size) = request.min_size {
            query.append_pair("min_size", &min_size.to_string());
        }
        if let Some(max_size) = request.max_size {
            query.append_pair("max_size", &max_size.to_string());
        }
    }
    Ok(url.to_string())
}

fn build_api_size_probe_url(
    config: &AniNzbConfig,
    request: &ApiSearchQuery,
    order: &str,
) -> Result<String, Error> {
    let mut url = Url::parse(&build_api_search_url(config, request)?)
        .map_err(|error| Error::msg(format!("invalid AniNZB size probe URL: {error}")))?;
    url.query_pairs_mut()
        .append_pair("sort", "size")
        .append_pair("order", order);
    Ok(url.to_string())
}

fn result_identity(result: &SearchResult) -> String {
    result
        .guid
        .as_deref()
        .or(result.download_url.as_deref())
        .or(result.link.as_deref())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| result.title.clone())
}

fn request_is_movie_shaped(request: &SearchRequest) -> bool {
    request
        .context
        .as_ref()
        .is_some_and(|context| context.subject_kind == PluginSearchSubjectKind::Movie)
        || request
            .facet
            .as_deref()
            .is_some_and(|facet| facet.trim().eq_ignore_ascii_case("movie"))
}

fn request_is_anime_shaped(request: &SearchRequest) -> bool {
    request
        .context
        .as_ref()
        .is_some_and(|context| context.subject_kind == PluginSearchSubjectKind::AnimeEpisode)
        || request
            .facet
            .as_deref()
            .is_some_and(|facet| facet.trim().eq_ignore_ascii_case("anime"))
}

fn request_id(request: &SearchRequest, key: &str) -> Option<String> {
    request
        .ids
        .get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn search_name(request: &SearchRequest) -> Option<String> {
    let query = request.query.trim();
    if !query.is_empty() {
        if request.season.is_some()
            || request.episode.is_some()
            || request.absolute_episode.is_some()
        {
            if let Some(alias) = request
                .tagged_aliases
                .iter()
                .map(|alias| alias.name.trim())
                .filter(|alias| !alias.is_empty())
                .filter(|alias| {
                    query
                        .get(..alias.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(alias))
                        && query.get(alias.len()..).is_some_and(|suffix| {
                            suffix.is_empty() || suffix.starts_with([' ', '.', '-', '_'])
                        })
                })
                .max_by_key(|alias| alias.len())
            {
                return Some(alias.to_string());
            }
            if let Some(base_name) = strip_search_scope_suffix(query, request) {
                return Some(base_name.to_string());
            }
        }
        return Some(query.to_string());
    }
    request
        .tagged_aliases
        .iter()
        .map(|alias| alias.name.trim())
        .find(|alias| !alias.is_empty())
        .map(ToOwned::to_owned)
}

fn strip_search_scope_suffix<'a>(query: &'a str, request: &SearchRequest) -> Option<&'a str> {
    let mut suffixes = Vec::new();
    if let (Some(season), Some(episode)) = (request.season, request.episode) {
        suffixes.push(format!(" S{season:02}E{episode:02}"));
        suffixes.push(format!(" S{season}E{episode}"));
    }
    if let Some(season) = request.season {
        suffixes.push(format!(" S{season:02}"));
        suffixes.push(format!(" S{season}"));
    }
    if let Some(absolute_episode) = request.absolute_episode {
        suffixes.push(format!(" {absolute_episode:03}"));
        suffixes.push(format!(" {absolute_episode}"));
    }

    suffixes.into_iter().find_map(|suffix| {
        let split_at = query.len().checked_sub(suffix.len())?;
        query
            .get(split_at..)
            .filter(|candidate| candidate.eq_ignore_ascii_case(&suffix))
            .and_then(|_| query.get(..split_at))
            .map(str::trim_end)
            .filter(|base_name| !base_name.is_empty())
    })
}

fn api_item_to_search_result(item: &AniNzbApiItem) -> Option<SearchResult> {
    let title = required_text(item.filename.as_deref())?;
    let download_url = api_download_url(item.nzb.as_deref())?;
    let source = item
        .source
        .as_deref()
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .unwrap_or("unknown")
        .to_ascii_lowercase();

    let mut external_ids = HashMap::new();
    if let Some(anidb_id) = item.anidb {
        external_ids.insert("anidb_id".to_string(), anidb_id.to_string());
    }
    if let Some(tvdb_id) = required_text(item.tvdb.as_deref()) {
        external_ids.insert("tvdb_id".to_string(), tvdb_id.to_string());
    }

    let mut provider_extra = HashMap::new();
    provider_extra.insert(
        "source".to_string(),
        serde_json::Value::from(source.clone()),
    );
    if let Some(id) = item.id {
        provider_extra.insert("api_item_id".to_string(), serde_json::Value::from(id));
    }
    insert_string_list(
        &mut provider_extra,
        "series_names",
        item.series_name.as_deref(),
    );
    insert_optional_text(&mut provider_extra, "group", item.group.as_deref());
    if let Some(season) = item.season {
        provider_extra.insert("season".to_string(), serde_json::Value::from(season));
    }
    if let Some(episode) = item.episode {
        provider_extra.insert("episode".to_string(), serde_json::Value::from(episode));
    }
    insert_optional_text(&mut provider_extra, "poster", item.poster.as_deref());
    insert_string_list(&mut provider_extra, "subtitles", item.subtitles.as_deref());
    insert_string_list(
        &mut provider_extra,
        "screenshots",
        item.screenshots.as_deref(),
    );
    insert_string_list(
        &mut provider_extra,
        "thumbnails",
        item.thumbnails.as_deref(),
    );

    let guid = item
        .id
        .map(|id| format!("aninzb:{source}:{id}"))
        .unwrap_or_else(|| format!("aninzb:{source}:{download_url}"));
    Some(SearchResult {
        title: title.to_string(),
        link: Some(download_url.clone()),
        download_url: Some(download_url),
        size_bytes: item.size,
        published_at: item.date.map(format_unix_timestamp),
        provider_extra,
        guid: Some(guid),
        source_kind: Some(IndexerSourceKind::Usenet),
        protocol: Some(IndexerProtocol::Usenet),
        external_ids,
        categories: vec!["5070".to_string()],
        provider_categories: vec!["TV/Anime".to_string()],
        ..SearchResult::default()
    })
}

fn required_text(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn api_download_url(value: Option<&str>) -> Option<String> {
    let url = Url::parse(required_text(value)?).ok()?;
    (url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(ANINZB_API_HOST))
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

fn insert_optional_text(
    provider_extra: &mut HashMap<String, serde_json::Value>,
    key: &str,
    value: Option<&str>,
) {
    if let Some(value) = required_text(value) {
        provider_extra.insert(key.to_string(), serde_json::Value::from(value));
    }
}

fn insert_string_list(
    provider_extra: &mut HashMap<String, serde_json::Value>,
    key: &str,
    values: Option<&[String]>,
) {
    if let Some(values) = values.filter(|values| !values.is_empty()) {
        provider_extra.insert(key.to_string(), serde_json::Value::from(values.to_vec()));
    }
}

fn config_optional(key: &str) -> Option<String> {
    component::config_get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn format_unix_timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

scryer_plugin_pdk::scryer_indexer_component_main!(
    descriptor = build_descriptor,
    search = search,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_config() -> LegacyAniNzbConfig {
        LegacyAniNzbConfig {
            base_url: Some("https://aninzb.moe".to_string()),
            api_key_present: true,
            api_path: Some("/api".to_string()),
            additional_params: Some("&legacy=1".to_string()),
            hourly_hit_cap: Some("1".to_string()),
            daily_hit_cap: Some("2".to_string()),
        }
    }

    #[test]
    fn descriptor_keeps_only_the_legacy_base_url_configuration() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        assert_eq!(indexer.config_fields.len(), 1);
        let base_url = &indexer.config_fields[0];
        assert_eq!(base_url.key, "base_url");
        assert_eq!(base_url.role, Some(ConfigFieldRole::ConnectionUrl));
        assert_eq!(
            base_url.default_value.as_deref(),
            Some(LEGACY_BASE_URL_DEFAULT)
        );
        assert_eq!(indexer.allowed_hosts, vec![ANINZB_API_HOST.to_string()]);
        assert_eq!(indexer.rate_limit_seconds, None);
        assert_eq!(indexer.capabilities.query_param.as_deref(), Some("name"));
        assert_eq!(indexer.capabilities.season_param.as_deref(), Some("season"));
        assert_eq!(
            indexer.capabilities.episode_param.as_deref(),
            Some("episode")
        );
        assert!(indexer.capabilities.rss);
        assert_eq!(
            indexer.capabilities.feed_modes,
            vec![
                IndexerFeedMode::Recent,
                IndexerFeedMode::Rss,
                IndexerFeedMode::AutomaticSearch,
                IndexerFeedMode::InteractiveSearch,
            ]
        );
        let limits = indexer.capabilities.limits.expect("limits");
        assert_eq!(limits.page_size, Some(API_MAX_RESULTS as u32));
        assert_eq!(limits.max_page_size, Some(API_MAX_RESULTS as u32));
        assert_eq!(limits.max_pages, Some(1));
        assert_eq!(limits.rate_limit_hint_seconds, None);
        assert!(!limits.api_quota_supported);
        let features = indexer
            .capabilities
            .response_features
            .expect("response features");
        assert!(features.guid);
        assert!(features.raw_provider_metadata);
        assert!(!features.info_url);
        assert!(!features.grabs);
        assert!(!features.comments);
    }

    #[test]
    fn legacy_config_is_ignored_in_favor_of_fixed_api_behavior() {
        let config = migrate_legacy_config(legacy_config());

        assert_eq!(config.api_base_url, ANINZB_API_BASE_URL);
        assert_eq!(config.http_behavior.user_agent, USER_AGENT);
        assert!(
            USER_AGENT
                .chars()
                .all(|character| !matches!(character, '\r' | '\n' | '\\'))
        );
        assert_eq!(config.http_behavior.pre_request_delay, Duration::ZERO);
        assert_eq!(API_REQUESTS_PER_SECOND, 3);
        assert_eq!(config.http_behavior.max_search_pages, 1);
        let budget = config.http_behavior.hit_budget.expect("hit budget");
        assert_eq!(budget.hourly_limit, DEFAULT_HOURLY_HIT_CAP);
        assert_eq!(budget.daily_limit, DEFAULT_DAILY_HIT_CAP);
    }

    #[test]
    fn api_queries_fan_out_ids_and_keep_anidb_season_scoped() {
        let request = SearchRequest {
            query: "Example Animation & Companions".to_string(),
            ids: HashMap::from([
                ("anidb_id".to_string(), "14758".to_string()),
                ("tvdb_id".to_string(), "371310".to_string()),
            ]),
            facet: Some("anime".to_string()),
            season: Some(2),
            episode: Some(4),
            absolute_episode: Some(55),
            ..SearchRequest::default()
        };
        let config = migrate_legacy_config(LegacyAniNzbConfig::default());
        let queries = initial_api_queries(&request);
        assert_eq!(queries.len(), 4);

        let anidb_query = queries
            .iter()
            .find(|query| matches!(query.branch, ApiSearchBranch::AniDb(_)))
            .expect("AniDB query");
        let anidb_url = Url::parse(&build_api_search_url(&config, anidb_query).unwrap()).unwrap();
        let anidb = anidb_url.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(
            anidb.get("anidb").map(|value| value.as_ref()),
            Some("14758")
        );
        assert_eq!(anidb.get("season").map(|value| value.as_ref()), None);
        assert_eq!(anidb.get("episode").map(|value| value.as_ref()), Some("55"));

        let tvdb_query = queries
            .iter()
            .find(|query| matches!(query.branch, ApiSearchBranch::Tvdb(_)))
            .expect("TVDB query");
        let tvdb_url = Url::parse(&build_api_search_url(&config, tvdb_query).unwrap()).unwrap();
        let tvdb = tvdb_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(tvdb.get("tvdb").map(|value| value.as_ref()), Some("371310"));
        assert_eq!(tvdb.get("season").map(|value| value.as_ref()), Some("2"));

        let name_query = queries
            .iter()
            .find(|query| matches!(query.branch, ApiSearchBranch::Name(_)))
            .expect("name query");
        let name_url = Url::parse(&build_api_search_url(&config, name_query).unwrap()).unwrap();
        let name = name_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            name.get("name").map(|value| value.as_ref()),
            Some("Example Animation & Companions")
        );

        let filename_query = queries
            .iter()
            .find(|query| matches!(query.branch, ApiSearchBranch::Filename(_)))
            .expect("filename query");
        let filename_url =
            Url::parse(&build_api_search_url(&config, filename_query).unwrap()).unwrap();
        let filename = filename_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            filename.get("filename").map(|value| value.as_ref()),
            Some("Example Animation & Companions")
        );

        let probe_url =
            Url::parse(&build_api_size_probe_url(&config, anidb_query, "asc").unwrap()).unwrap();
        let probe = probe_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(probe.get("sort").map(|value| value.as_ref()), Some("size"));
        assert_eq!(probe.get("order").map(|value| value.as_ref()), Some("asc"));
    }

    #[test]
    fn season_pack_queries_keep_structured_and_filename_scopes_distinct() {
        let request = SearchRequest {
            query: "Example Animation & Companions S06".to_string(),
            ids: HashMap::from([
                ("anidb_id".to_string(), "14758".to_string()),
                ("tvdb_id".to_string(), "371310".to_string()),
            ]),
            facet: Some("anime".to_string()),
            season: Some(6),
            ..SearchRequest::default()
        };
        let config = migrate_legacy_config(LegacyAniNzbConfig::default());
        let queries = initial_api_queries(&request);
        assert_eq!(queries.len(), 4);

        let anidb_query = queries
            .iter()
            .find(|query| matches!(query.branch, ApiSearchBranch::AniDb(_)))
            .expect("AniDB query");
        let anidb_url = Url::parse(&build_api_search_url(&config, anidb_query).unwrap()).unwrap();
        let anidb = anidb_url.query_pairs().collect::<HashMap<_, _>>();
        assert!(!anidb.contains_key("season"));

        let tvdb_query = queries
            .iter()
            .find(|query| matches!(query.branch, ApiSearchBranch::Tvdb(_)))
            .expect("TVDB query");
        let tvdb_url = Url::parse(&build_api_search_url(&config, tvdb_query).unwrap()).unwrap();
        let tvdb = tvdb_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(tvdb.get("season").map(|value| value.as_ref()), Some("6"));

        let filename_query = queries
            .iter()
            .find(|query| matches!(query.branch, ApiSearchBranch::ScopedFilename { .. }))
            .expect("scoped filename query");
        let filename_url =
            Url::parse(&build_api_search_url(&config, filename_query).unwrap()).unwrap();
        let filename = filename_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            filename.get("name").map(|value| value.as_ref()),
            Some("Example Animation & Companions")
        );
        assert_eq!(
            filename.get("filename").map(|value| value.as_ref()),
            Some("S06")
        );
        assert!(!filename.contains_key("season"));
    }

    #[test]
    fn api_url_for_recent_search_uses_newest_release_source() {
        let config = migrate_legacy_config(LegacyAniNzbConfig::default());
        let query = initial_api_queries(&SearchRequest::default())
            .pop()
            .unwrap();
        let url = Url::parse(&build_api_search_url(&config, &query).unwrap()).unwrap();
        let query = url.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(
            query.get("source").map(|value| value.as_ref()),
            Some("release")
        );
    }

    #[test]
    fn saturated_responses_partition_only_by_non_overlapping_size_ranges() {
        let query = ApiSearchQuery {
            branch: ApiSearchBranch::AniDb("14758".to_string()),
            season: None,
            episode: None,
            min_size: None,
            max_size: None,
            partition_depth: 0,
        };
        let items = [100, 200, 300]
            .into_iter()
            .map(|size| AniNzbApiItem {
                source: None,
                id: None,
                filename: None,
                anidb: None,
                series_name: None,
                episode: None,
                season: None,
                tvdb: None,
                size: Some(size),
                group: None,
                date: None,
                nzb: None,
                poster: None,
                subtitles: None,
                screenshots: None,
                thumbnails: None,
            })
            .collect::<Vec<_>>();

        let (lower, upper) = query.size_partitions(&items).expect("partitionable");
        assert_eq!(lower.max_size, Some(200));
        assert_eq!(upper.min_size, Some(201));
        assert!(matches!(lower.branch, ApiSearchBranch::AniDb(_)));
        assert!(matches!(upper.branch, ApiSearchBranch::AniDb(_)));
    }

    #[test]
    fn saturated_response_with_an_unsplittable_size_range_stays_incomplete() {
        let query = ApiSearchQuery {
            branch: ApiSearchBranch::Tvdb("371310".to_string()),
            season: Some(2),
            episode: None,
            min_size: Some(1_000),
            max_size: Some(1_000),
            partition_depth: 0,
        };
        let item = AniNzbApiItem {
            source: None,
            id: None,
            filename: None,
            anidb: None,
            series_name: None,
            episode: None,
            season: None,
            tvdb: None,
            size: Some(1_000),
            group: None,
            date: None,
            nzb: None,
            poster: None,
            subtitles: None,
            screenshots: None,
            thumbnails: None,
        };
        assert!(query.size_partitions(&[item]).is_none());
    }

    #[test]
    fn api_page_recognizes_reported_or_returned_saturation() {
        assert!(
            ApiSearchPage {
                total_count: Some(API_MAX_RESULTS as u64),
                items: Vec::new(),
            }
            .is_saturated()
        );
        assert!(
            ApiSearchPage {
                total_count: None,
                items: vec![
                    AniNzbApiItem {
                        source: None,
                        id: None,
                        filename: None,
                        anidb: None,
                        series_name: None,
                        episode: None,
                        season: None,
                        tvdb: None,
                        size: None,
                        group: None,
                        date: None,
                        nzb: None,
                        poster: None,
                        subtitles: None,
                        screenshots: None,
                        thumbnails: None,
                    };
                    API_MAX_RESULTS
                ],
            }
            .is_saturated()
        );
        assert!(
            !ApiSearchPage {
                total_count: Some(API_MAX_RESULTS as u64),
                items: vec![AniNzbApiItem::default(); API_MAX_RESULTS],
            }
            .is_saturated()
        );
    }

    #[test]
    fn movie_requests_are_unsupported() {
        let request = SearchRequest {
            facet: Some("movie".to_string()),
            ..SearchRequest::default()
        };
        assert!(request_is_movie_shaped(&request));
    }

    #[test]
    fn api_item_maps_release_metadata_and_artifacts() {
        let body = r#"{
          "total_count": 1,
          "items": [{
            "source": "release", "id": 10936,
            "filename": "Example.Animation.S03E01.1080p-VARYG",
            "anidb": 14758, "series_name": ["Example Animation", "Example Localized Name"],
            "episode": 1.0, "season": 3, "tvdb": "371310",
            "size": 1640102917, "group": "VARYG", "date": 0,
            "nzb": "https://api.aninzb.moe/releases/10936/release.nzb",
            "poster": "https://api.aninzb.moe/posters/14758",
            "subtitles": ["https://api.aninzb.moe/subtitles/1.ass"],
            "screenshots": ["https://api.aninzb.moe/screenshots/1.png"],
            "thumbnails": ["https://api.aninzb.moe/thumbnails/1.jpg"]
          }]
        }"#;
        let response: AniNzbApiResponse = serde_json::from_str(body).unwrap();
        let result = api_item_to_search_result(&response.items.as_ref().expect("items")[0])
            .expect("usable result");

        assert_eq!(result.title, "Example.Animation.S03E01.1080p-VARYG");
        assert_eq!(
            result.download_url.as_deref(),
            Some("https://api.aninzb.moe/releases/10936/release.nzb")
        );
        assert_eq!(result.guid.as_deref(), Some("aninzb:release:10936"));
        assert_eq!(result.size_bytes, Some(1_640_102_917));
        assert_eq!(result.published_at.as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(result.source_kind, Some(IndexerSourceKind::Usenet));
        assert_eq!(result.protocol, Some(IndexerProtocol::Usenet));
        assert_eq!(
            result.external_ids.get("anidb_id").map(String::as_str),
            Some("14758")
        );
        assert_eq!(
            result.provider_extra.get("subtitles"),
            Some(&serde_json::json!([
                "https://api.aninzb.moe/subtitles/1.ass"
            ]))
        );
    }

    #[test]
    fn api_item_accepts_null_optional_fields_and_all_sources() {
        for source in ["release", "tosho", "usenet"] {
            let item: AniNzbApiItem = serde_json::from_value(serde_json::json!({
                "source": source,
                "filename": "Example.mkv",
                "nzb": "https://api.aninzb.moe/example.nzb",
                "anidb": null, "series_name": null, "episode": null,
                "season": null, "tvdb": null, "size": null, "group": null,
                "date": null, "poster": null, "subtitles": null,
                "screenshots": null, "thumbnails": null
            }))
            .unwrap();
            let result = api_item_to_search_result(&item).expect("usable result");
            let expected_guid = format!("aninzb:{source}:https://api.aninzb.moe/example.nzb");
            assert_eq!(result.guid.as_deref(), Some(expected_guid.as_str()));
            assert_eq!(result.published_at, None);
            assert_eq!(result.source_kind, Some(IndexerSourceKind::Usenet));
        }
    }

    #[test]
    fn api_item_skips_missing_acquisition_fields() {
        let missing_filename: AniNzbApiItem = serde_json::from_value(serde_json::json!({
            "nzb": "https://api.aninzb.moe/example.nzb"
        }))
        .unwrap();
        let missing_nzb: AniNzbApiItem = serde_json::from_value(serde_json::json!({
            "filename": "Example.mkv"
        }))
        .unwrap();

        assert!(api_item_to_search_result(&missing_filename).is_none());
        assert!(api_item_to_search_result(&missing_nzb).is_none());
    }

    #[test]
    fn api_item_rejects_non_aninzb_download_urls() {
        let item: AniNzbApiItem = serde_json::from_value(serde_json::json!({
            "filename": "Example.mkv",
            "nzb": "https://localhost/private.nzb"
        }))
        .unwrap();

        assert!(api_item_to_search_result(&item).is_none());
    }

    #[test]
    fn api_response_accepts_null_items_as_an_empty_list() {
        let response = parse_api_response(r#"{"total_count": 0, "items": null}"#).unwrap();

        assert!(response.items.unwrap_or_default().is_empty());
    }

    #[test]
    fn api_response_size_is_limited_to_20_mib() {
        assert!(validate_api_response_size(MAX_API_RESPONSE_BYTES).is_ok());
        let error = validate_api_response_size(MAX_API_RESPONSE_BYTES + 1).unwrap_err();
        assert!(error.to_string().contains("exceeded 20971520 bytes"));
    }
}
