use std::collections::HashMap;
use std::time::Duration;

#[cfg(not(test))]
use newznab_common::hit_budget_snapshot;
use newznab_common::{
    Capabilities, ConfigFieldDef, IndexerCategoryModel, IndexerCategoryValueKind,
    IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol,
    IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind, NewznabConfig,
    NewznabHitBudget, NewznabHttpBehavior, PluginDescriptor, ProviderDescriptor, SDK_VERSION,
    SearchRequest, SearchResponse, current_sdk_constraint, execute_full_search, execute_raw_search,
    is_hit_budget_exhausted_error, standard_config_fields,
};
use scryer_plugin_pdk::*;
use serde_json::json;

const PROVIDER_ID: &str = "amenzb";
const AMENZB_BASE_URL: &str = "https://amenzb.moe";
const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;
const MAX_SEARCH_PAGES: usize = 2;
const DEFAULT_HOURLY_HIT_CAP: u32 = 450;
const DEFAULT_DAILY_HIT_CAP: u32 = 9_000;
const DEFAULT_CATEGORY: &str = "5070";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_ID.to_string(),
        name: "ameNZB Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: PROVIDER_ID.to_string(),
            provider_aliases: vec![],
            provider_profiles: vec![],
            search_semantics_version: Some(2),
            strategy_plan: Some(scryer_plugin_sdk::IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 4,
            }),
            source_kind: IndexerSourceKind::Usenet,
            capabilities: Capabilities {
                supported_ids: HashMap::from([
                    (
                        "anime".into(),
                        vec![
                            "anidb_id".into(),
                            "anidb".into(),
                            "tvdb_id".into(),
                            "info_hash".into(),
                            "info_hash_v1".into(),
                            "btih".into(),
                        ],
                    ),
                    (
                        "series".into(),
                        vec![
                            "tvdb_id".into(),
                            "tmdb_id".into(),
                            "imdb_id".into(),
                            "info_hash".into(),
                            "info_hash_v1".into(),
                            "btih".into(),
                        ],
                    ),
                    (
                        "movie".into(),
                        vec!["tmdb_id".into(), "imdb_id".into(), "info_hash".into()],
                    ),
                ]),
                deduplicates_aliases: false,
                season_param: Some("season".into()),
                episode_param: Some("ep".into()),
                query_param: Some("q".into()),
                supported_query_facets: vec!["anime".into(), "series".into(), "movie".into()],
                search: true,
                imdb_search: true,
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
                    IndexerSearchInput::Category,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec![
                    "anidb_id".into(),
                    "anidb".into(),
                    "tvdb_id".into(),
                    "tmdb_id".into(),
                    "imdb_id".into(),
                    "info_hash".into(),
                    "info_hash_v1".into(),
                    "btih".into(),
                ],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(DEFAULT_PAGE_SIZE as u32),
                    max_page_size: Some(MAX_PAGE_SIZE as u32),
                    max_pages: Some(MAX_SEARCH_PAGES as u32),
                    api_quota_supported: true,
                    grab_quota_supported: true,
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: None,
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    grabs: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec![],
            rate_limit_seconds: Some(1),
        }),
    }
}

async fn search(req: SearchRequest) -> FnResult<SearchResponse> {
    let ame_config = AmeConfig::from_host()?;

    let response = if let Some(info_hash) = request_id(&req, "info_hash")
        .or_else(|| request_id(&req, "info_hash_v1"))
        .or_else(|| request_id(&req, "btih"))
    {
        let raw_req = req_for_exact_provider_filter(&req);
        let config = ame_config.newznab_config(provider_params(
            &raw_req,
            vec![("info_hash".to_string(), info_hash)],
        ));
        execute_raw_search_gracefully(&config, &raw_req).await?
    } else if let Some(anidb_id) =
        request_id(&req, "anidb_id").or_else(|| request_id(&req, "anidb"))
    {
        let raw_req = req_for_exact_provider_filter(&req);
        let config =
            ame_config.newznab_config(provider_params(&raw_req, anime_id_pairs(&req, anidb_id)));
        execute_raw_search_gracefully(&config, &raw_req).await?
    } else {
        let req = normalize_request_ids(req);
        let config = ame_config.newznab_config(provider_params(&req, Vec::new()));
        execute_full_search(&config, &req, amenzb_metadata_extractor).await?
    };

    Ok(response)
}

async fn execute_raw_search_gracefully(
    config: &NewznabConfig,
    req: &SearchRequest,
) -> Result<SearchResponse, Error> {
    match execute_raw_search(config, req, amenzb_metadata_extractor).await {
        Ok(response) => Ok(response),
        Err(error) if is_hit_budget_exhausted_error(&error) => empty_hit_budget_response(config),
        Err(error) => Err(error),
    }
}

#[cfg(not(test))]
fn empty_hit_budget_response(config: &NewznabConfig) -> Result<SearchResponse, Error> {
    let (api_current, api_max) = hit_budget_snapshot(&config.http_behavior)?
        .map(|snapshot| snapshot.limiting_current_max())
        .unwrap_or((None, None));
    Ok(SearchResponse {
        results: vec![],
        api_current,
        api_max,
        grab_current: None,
        grab_max: None,
    })
}

scryer_indexer_component_main!(descriptor = build_descriptor, search = search,);

#[cfg(test)]
fn empty_hit_budget_response(config: &NewznabConfig) -> Result<SearchResponse, Error> {
    let (api_current, api_max) = config
        .http_behavior
        .hit_budget
        .as_ref()
        .map(|budget| (Some(0), Some(budget.hourly_limit.min(budget.daily_limit))))
        .unwrap_or((None, None));
    Ok(SearchResponse {
        results: vec![],
        api_current,
        api_max,
        grab_current: None,
        grab_max: None,
    })
}

#[derive(Debug, Clone)]
struct AmeConfig {
    base_url: String,
    api_key: String,
}

impl AmeConfig {
    fn from_host() -> Result<Self, Error> {
        let mut base = NewznabConfig::from_host()?;
        if base.base_url.trim().is_empty() {
            base.base_url = AMENZB_BASE_URL.to_string();
        }
        Ok(Self {
            base_url: base.base_url,
            api_key: base.api_key,
        })
    }

    fn newznab_config(&self, provider_params: String) -> NewznabConfig {
        let mut config = NewznabConfig {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            api_path: "/api".to_string(),
            additional_params: provider_params,
            page_size: DEFAULT_PAGE_SIZE,
            http_behavior: NewznabHttpBehavior::default(),
        };
        apply_amenzb_http_behavior(&mut config);
        config
    }
}

fn apply_amenzb_http_behavior(config: &mut NewznabConfig) {
    config.http_behavior = NewznabHttpBehavior {
        plugin_id: PROVIDER_ID.to_string(),
        user_agent: USER_AGENT.to_string(),
        pre_request_delay: Duration::from_millis(250),
        retry_total_budget: Duration::from_secs(30),
        retry_default_delay: Duration::from_secs(30),
        retry_max_delay: Duration::from_secs(120),
        retry_max_attempts: 2,
        max_search_pages: MAX_SEARCH_PAGES,
        hit_budget: Some(NewznabHitBudget {
            var_key: "amenzb.http_hits".to_string(),
            hourly_limit: DEFAULT_HOURLY_HIT_CAP,
            daily_limit: DEFAULT_DAILY_HIT_CAP,
        }),
    };
}

fn config_fields() -> Vec<ConfigFieldDef> {
    let mut fields = standard_config_fields(Some(AMENZB_BASE_URL));
    fields.retain(|field| matches!(field.key.as_str(), "base_url" | "api_key"));
    require_api_key(&mut fields);
    fields
}

fn require_api_key(fields: &mut [ConfigFieldDef]) {
    if let Some(field) = fields.iter_mut().find(|field| field.key == "api_key") {
        field.required = true;
        field.help_text = Some(
            "ameNZB API key. Required; ameNZB keys are also pinned to the caller IP.".to_string(),
        );
    }
}

fn normalize_request_ids(mut req: SearchRequest) -> SearchRequest {
    if request_id(&req, "anidb_id").is_none()
        && let Some(anidb) = request_id(&req, "anidb")
    {
        req.ids.insert("anidb_id".to_string(), anidb);
    }
    req
}

fn req_for_exact_provider_filter(req: &SearchRequest) -> SearchRequest {
    let mut req = req.clone();
    req.query.clear();
    req.ids.clear();
    req.season = None;
    req.episode = None;
    req.absolute_episode = None;
    if req.categories.is_empty()
        && let Some(category) = provider_category_param(&req)
    {
        req.categories.push(category.to_string());
    }
    req
}

fn anime_id_pairs(req: &SearchRequest, anidb_id: String) -> Vec<(String, String)> {
    let mut pairs = vec![("anime_id".to_string(), anidb_id)];
    if let Some(season) = req.season {
        pairs.push(("season".to_string(), season.to_string()));
    }
    if let Some(episode) = req.absolute_episode.or(req.episode) {
        pairs.push(("ep".to_string(), episode.to_string()));
    }
    pairs
}

fn provider_params(
    req: &SearchRequest,
    extra_pairs: impl IntoIterator<Item = (String, String)>,
) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();

    if req.categories.is_empty()
        && let Some(category) = provider_category_param(req)
    {
        pairs.push(("cat".to_string(), category));
    }
    if let Some(anidb_id) = request_id(req, "anidb_id")
        .or_else(|| request_id(req, "anidb"))
        .filter(|value| !value.trim().is_empty())
    {
        pairs.push(("anime_id".to_string(), anidb_id));
    }
    pairs.extend(extra_pairs);

    encode_query_pairs(pairs)
}

fn provider_category_param(req: &SearchRequest) -> Option<String> {
    if !req.categories.is_empty() {
        return None;
    }

    match req
        .category
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) if is_newznab_category_param(value) => Some(value.to_string()),
        Some(value) if value.eq_ignore_ascii_case("anime") => Some(DEFAULT_CATEGORY.to_string()),
        Some(_) => None,
        None => Some(DEFAULT_CATEGORY.to_string()),
    }
}

fn is_newznab_category_param(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn encode_query_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(&key, &value);
    }
    let encoded = serializer.finish();
    if encoded.is_empty() {
        String::new()
    } else {
        format!("&{encoded}")
    }
}

fn amenzb_metadata_extractor(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut languages = Vec::new();
    let mut grabs = None;
    let mut extra = HashMap::new();

    for (name, value) in pairs {
        let normalized = normalize_attr_name(name);
        match normalized.as_str() {
            "language" | "audio" | "audiolang" => languages.extend(split_metadata_list(value)),
            "subs" | "subtitles" | "sublang" => {
                let values = split_metadata_list(value);
                if !values.is_empty() {
                    extra.insert("subtitle_languages".to_string(), json!(values));
                }
            }
            "grabs" => grabs = value.trim().replace(',', "").parse::<i64>().ok(),
            "category" | "resolution" | "source" | "season" | "episode" | "video" | "guid"
            | "releasegroup" | "translation"
                if !value.trim().is_empty() =>
            {
                extra.insert(normalized, json!(value.trim()));
            }
            _ => {}
        }
    }

    (languages, grabs, extra)
}

fn normalize_attr_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn split_metadata_list(value: &str) -> Vec<String> {
    value
        .split([',', '/', '|'])
        .flat_map(|part| part.split(" - "))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn request_id(req: &SearchRequest, key: &str) -> Option<String> {
    req.ids
        .get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> SearchRequest {
        SearchRequest {
            query: "Example Animation".to_string(),
            ids: HashMap::new(),
            facet: None,
            category: None,
            categories: vec![],
            limit: 0,
            season: None,
            episode: None,
            absolute_episode: None,
            tagged_aliases: vec![],
            context: None,
        }
    }

    fn ame_config() -> AmeConfig {
        AmeConfig {
            base_url: AMENZB_BASE_URL.to_string(),
            api_key: "secret".to_string(),
        }
    }

    #[test]
    fn descriptor_advertises_amenzb_specific_ids_and_quotas() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        assert_eq!(indexer.provider_type, PROVIDER_ID);
        assert_eq!(indexer.source_kind, IndexerSourceKind::Usenet);
        assert!(indexer.capabilities.supported_ids["anime"].contains(&"anidb_id".to_string()));
        assert!(indexer.capabilities.supported_ids["anime"].contains(&"btih".to_string()));
        let limits = indexer.capabilities.limits.expect("limits");
        assert_eq!(limits.page_size, Some(DEFAULT_PAGE_SIZE as u32));
        assert_eq!(limits.max_page_size, Some(MAX_PAGE_SIZE as u32));
        assert!(limits.api_quota_supported);
        assert!(limits.grab_quota_supported);
        let api_key = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "api_key")
            .expect("api key field");
        assert!(api_key.required);
        assert_eq!(
            indexer
                .config_fields
                .iter()
                .map(|field| field.key.as_str())
                .collect::<Vec<_>>(),
            vec!["base_url", "api_key"]
        );
    }

    #[test]
    fn provider_params_include_anidb_and_category_without_provider_preferences() {
        let mut req = request();
        req.ids.insert("anidb".to_string(), "12345".to_string());

        let params = provider_params(&req, Vec::new());

        assert!(params.starts_with('&'));
        assert!(params.contains("cat=5070"));
        assert!(params.contains("anime_id=12345"));
        assert!(!params.contains("healthy="));
        assert!(!params.contains("audio_lang="));
        assert!(!params.contains("sub_lang="));
        assert!(!params.contains("translation="));
        assert!(!params.contains("source="));
        assert!(!params.contains("resolution="));
        assert!(!params.contains("release_group="));
        assert!(!params.contains("season="));
        assert!(!params.contains("ep="));
    }

    #[test]
    fn provider_params_do_not_duplicate_category_when_request_has_categories() {
        let mut req = request();
        req.categories.push("2000".to_string());

        let params = provider_params(&req, Vec::new());

        assert!(!params.contains("cat=5070"));
    }

    #[test]
    fn provider_params_respect_singular_category_hint() {
        let mut movie_req = request();
        movie_req.category = Some("movie".to_string());
        let movie_params = provider_params(&movie_req, Vec::new());
        assert!(!movie_params.contains("cat=5070"));

        let mut numeric_req = request();
        numeric_req.category = Some("2000".to_string());
        let numeric_params = provider_params(&numeric_req, Vec::new());
        assert!(numeric_params.contains("cat=2000"));

        let mut anime_req = request();
        anime_req.category = Some("anime".to_string());
        let anime_params = provider_params(&anime_req, Vec::new());
        assert!(anime_params.contains("cat=5070"));
    }

    #[test]
    fn exact_hash_request_clears_broad_search_inputs_but_keeps_default_category() {
        let mut req = request();
        req.ids
            .insert("info_hash".to_string(), "ABCDEF".to_string());
        req.season = Some(1);
        req.episode = Some(2);

        let raw = req_for_exact_provider_filter(&req);

        assert!(raw.query.is_empty());
        assert!(raw.ids.is_empty());
        assert_eq!(raw.categories, vec![DEFAULT_CATEGORY.to_string()]);
        assert_eq!(raw.season, None);
        assert_eq!(raw.episode, None);
    }

    #[test]
    fn direct_anidb_params_survive_raw_request_shape() {
        let mut req = request();
        req.ids.insert("anidb_id".to_string(), "12345".to_string());
        req.season = Some(2);
        req.absolute_episode = Some(12);

        let raw = req_for_exact_provider_filter(&req);
        let params = provider_params(&raw, anime_id_pairs(&req, "12345".to_string()));

        assert!(raw.query.is_empty());
        assert!(raw.ids.is_empty());
        assert_eq!(raw.season, None);
        assert_eq!(raw.absolute_episode, None);
        assert_eq!(raw.categories, vec![DEFAULT_CATEGORY.to_string()]);
        assert!(!params.contains("cat=5070"));
        assert!(params.contains("anime_id=12345"));
        assert!(params.contains("season=2"));
        assert!(params.contains("ep=12"));
    }

    #[test]
    fn metadata_extractor_splits_ame_language_and_subtitle_attrs() {
        let pairs = vec![
            ("language".to_string(), "Japanese, English".to_string()),
            ("subs".to_string(), "English / Spanish".to_string()),
            ("resolution".to_string(), "2160p".to_string()),
            ("source".to_string(), "WEB".to_string()),
            ("grabs".to_string(), "1,234".to_string()),
        ];

        let (languages, grabs, extra) = amenzb_metadata_extractor(&pairs);

        assert_eq!(languages, vec!["Japanese", "English"]);
        assert_eq!(grabs, Some(1234));
        assert_eq!(extra["subtitle_languages"], json!(["English", "Spanish"]));
        assert_eq!(extra["resolution"], json!("2160p"));
        assert_eq!(extra["source"], json!("WEB"));
    }

    #[test]
    fn newznab_config_uses_fixed_provider_defaults() {
        let config = ame_config().newznab_config("&anime_id=12345".to_string());

        assert_eq!(config.base_url, AMENZB_BASE_URL);
        assert_eq!(config.api_path, "/api");
        assert_eq!(config.additional_params, "&anime_id=12345");
        assert_eq!(config.page_size, DEFAULT_PAGE_SIZE);
        assert_eq!(config.http_behavior.plugin_id, PROVIDER_ID);
        assert_eq!(config.http_behavior.user_agent, USER_AGENT);
        assert_eq!(config.http_behavior.max_search_pages, MAX_SEARCH_PAGES);
        assert_eq!(
            config.http_behavior.pre_request_delay,
            Duration::from_millis(250)
        );
        assert_eq!(config.http_behavior.retry_max_attempts, 2);
        let budget = config
            .http_behavior
            .hit_budget
            .as_ref()
            .expect("hit budget");
        assert_eq!(budget.hourly_limit, DEFAULT_HOURLY_HIT_CAP);
        assert_eq!(budget.daily_limit, DEFAULT_DAILY_HIT_CAP);
        assert!(budget.daily_limit < 10_000);
    }

    #[test]
    fn local_hit_budget_empty_response_is_successful_and_empty() {
        let config = NewznabConfig {
            base_url: AMENZB_BASE_URL.to_string(),
            api_key: String::new(),
            api_path: "/api".to_string(),
            additional_params: String::new(),
            page_size: DEFAULT_PAGE_SIZE,
            http_behavior: NewznabHttpBehavior {
                hit_budget: None,
                ..NewznabHttpBehavior::default()
            },
        };

        let response = empty_hit_budget_response(&config).expect("empty response");

        assert!(response.results.is_empty());
        assert_eq!(response.api_current, None);
        assert_eq!(response.api_max, None);
        assert_eq!(response.grab_current, None);
        assert_eq!(response.grab_max, None);
    }
}
