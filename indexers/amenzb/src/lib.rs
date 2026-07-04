use std::collections::HashMap;
use std::time::Duration;

use extism_pdk::*;
use newznab_common::{
    Capabilities, ConfigFieldDef, ConfigFieldType, IndexerCategoryModel, IndexerCategoryValueKind,
    IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol,
    IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind, NewznabConfig,
    NewznabHitBudget, NewznabHttpBehavior, PluginDescriptor, PluginResult, ProviderDescriptor,
    SDK_VERSION, SearchRequest, current_sdk_constraint, execute_full_search, execute_raw_search,
    standard_config_fields,
};
use serde_json::json;

const PROVIDER_ID: &str = "amenzb";
const AMENZB_BASE_URL: &str = "https://amenzb.moe";
const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;
const MAX_SEARCH_PAGES: usize = 2;
const DEFAULT_DAILY_HIT_CAP: u32 = 9_000;
const DEFAULT_CATEGORY: &str = "5070";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
}

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

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let ame_config = AmeConfig::from_extism()?;

    let response = if let Some(info_hash) = request_id(&req, "info_hash")
        .or_else(|| request_id(&req, "info_hash_v1"))
        .or_else(|| request_id(&req, "btih"))
    {
        let raw_req = req_for_exact_provider_filter(&req, &ame_config.category);
        let config = ame_config.newznab_config(provider_params(
            &ame_config,
            &raw_req,
            vec![("info_hash".to_string(), info_hash)],
        ));
        execute_raw_search(&config, &raw_req, amenzb_metadata_extractor)?
    } else if let Some(anidb_id) =
        request_id(&req, "anidb_id").or_else(|| request_id(&req, "anidb"))
    {
        let raw_req = req_for_exact_provider_filter(&req, &ame_config.category);
        let config = ame_config.newznab_config(provider_params(
            &ame_config,
            &raw_req,
            anime_id_pairs(&req, anidb_id),
        ));
        execute_raw_search(&config, &raw_req, amenzb_metadata_extractor)?
    } else {
        let req = normalize_request_ids(req);
        let config = ame_config.newznab_config(provider_params(&ame_config, &req, Vec::new()));
        execute_full_search(&config, &req, amenzb_metadata_extractor)?
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[derive(Debug, Clone)]
struct AmeConfig {
    base_url: String,
    api_key: String,
    api_path: String,
    additional_params: String,
    page_size: usize,
    category: Option<String>,
    healthy_only: bool,
    audio_lang: Option<String>,
    sub_lang: Option<String>,
    translation: Option<String>,
    source: Option<String>,
    resolution: Option<String>,
    release_group: Option<String>,
}

impl AmeConfig {
    fn from_extism() -> Result<Self, Error> {
        let mut base = NewznabConfig::from_extism()?;
        if base.base_url.trim().is_empty() {
            base.base_url = AMENZB_BASE_URL.to_string();
        }
        if base.api_path.trim().is_empty() {
            base.api_path = "/api".to_string();
        }
        base.page_size = effective_page_size(config_usize_optional("page_size"));

        Ok(Self {
            base_url: base.base_url,
            api_key: base.api_key,
            api_path: base.api_path,
            additional_params: base.additional_params,
            page_size: base.page_size,
            category: config_optional("category").or_else(|| Some(DEFAULT_CATEGORY.to_string())),
            healthy_only: config_bool("healthy_only", false),
            audio_lang: config_optional("audio_lang"),
            sub_lang: config_optional("sub_lang"),
            translation: config_optional("translation"),
            source: config_optional("source"),
            resolution: config_optional("resolution"),
            release_group: config_optional("release_group"),
        })
    }

    fn newznab_config(&self, provider_params: String) -> NewznabConfig {
        let mut config = NewznabConfig {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            api_path: self.api_path.clone(),
            additional_params: merge_query_params(&self.additional_params, &provider_params),
            page_size: self.page_size,
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
            hourly_limit: config_u32("hourly_hit_cap", 450),
            daily_limit: config_u32("daily_hit_cap", DEFAULT_DAILY_HIT_CAP),
        }),
    };
}

fn effective_page_size(configured: Option<usize>) -> usize {
    configured
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    let mut fields = standard_config_fields(Some(AMENZB_BASE_URL));
    require_api_key(&mut fields);
    push_number_field(
        &mut fields,
        "page_size",
        "Page Size",
        DEFAULT_PAGE_SIZE.to_string(),
        "Results requested per API page; hard-capped at ameNZB's API max of 100.",
    );
    push_string_field(
        &mut fields,
        "category",
        "Category",
        Some(DEFAULT_CATEGORY.to_string()),
        "Default Newznab category. 5070 is anime.",
    );
    push_bool_field(
        &mut fields,
        "healthy_only",
        "Healthy Only",
        Some("false".to_string()),
        "Send healthy=1 to filter for releases ameNZB considers healthy.",
    );
    push_string_field(
        &mut fields,
        "audio_lang",
        "Audio Language",
        None,
        "Optional ameNZB audio_lang filter.",
    );
    push_string_field(
        &mut fields,
        "sub_lang",
        "Subtitle Language",
        None,
        "Optional ameNZB sub_lang filter.",
    );
    push_string_field(
        &mut fields,
        "translation",
        "Translation",
        None,
        "Optional ameNZB translation filter, usually subbed or raw.",
    );
    push_string_field(
        &mut fields,
        "source",
        "Source",
        None,
        "Optional ameNZB source filter.",
    );
    push_string_field(
        &mut fields,
        "resolution",
        "Resolution",
        None,
        "Optional ameNZB resolution filter.",
    );
    push_string_field(
        &mut fields,
        "release_group",
        "Release Group",
        None,
        "Optional ameNZB release_group filter.",
    );
    push_number_field(
        &mut fields,
        "hourly_hit_cap",
        "Hourly Hit Cap",
        "450".to_string(),
        "Maximum ameNZB API requests per hour before searches return no results.",
    );
    push_number_field(
        &mut fields,
        "daily_hit_cap",
        "Daily Hit Cap",
        DEFAULT_DAILY_HIT_CAP.to_string(),
        "Maximum ameNZB API requests per day before searches return no results.",
    );
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

fn push_string_field(
    fields: &mut Vec<ConfigFieldDef>,
    key: &str,
    label: &str,
    default_value: Option<String>,
    help_text: &str,
) {
    fields.push(ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::String,
        required: false,
        default_value,
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options: vec![],
        help_text: Some(help_text.to_string()),
    });
}

fn push_number_field(
    fields: &mut Vec<ConfigFieldDef>,
    key: &str,
    label: &str,
    default_value: String,
    help_text: &str,
) {
    fields.push(ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::Number,
        required: false,
        default_value: Some(default_value),
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options: vec![],
        help_text: Some(help_text.to_string()),
    });
}

fn push_bool_field(
    fields: &mut Vec<ConfigFieldDef>,
    key: &str,
    label: &str,
    default_value: Option<String>,
    help_text: &str,
) {
    fields.push(ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::Bool,
        required: false,
        default_value,
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options: vec![],
        help_text: Some(help_text.to_string()),
    });
}

fn normalize_request_ids(mut req: SearchRequest) -> SearchRequest {
    if request_id(&req, "anidb_id").is_none() {
        if let Some(anidb) = request_id(&req, "anidb") {
            req.ids.insert("anidb_id".to_string(), anidb);
        }
    }
    req
}

fn req_for_exact_provider_filter(req: &SearchRequest, category: &Option<String>) -> SearchRequest {
    let mut req = req.clone();
    req.query.clear();
    req.ids.clear();
    req.season = None;
    req.episode = None;
    req.absolute_episode = None;
    if req.categories.is_empty() {
        if let Some(category) = provider_category_param(category, &req) {
            req.categories.push(category.to_string());
        }
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
    config: &AmeConfig,
    req: &SearchRequest,
    extra_pairs: impl IntoIterator<Item = (String, String)>,
) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();

    if req.categories.is_empty() {
        if let Some(category) = provider_category_param(&config.category, req) {
            pairs.push(("cat".to_string(), category));
        }
    }
    if config.healthy_only {
        pairs.push(("healthy".to_string(), "1".to_string()));
    }
    push_optional_pair(&mut pairs, "audio_lang", config.audio_lang.as_deref());
    push_optional_pair(&mut pairs, "sub_lang", config.sub_lang.as_deref());
    push_optional_pair(&mut pairs, "translation", config.translation.as_deref());
    push_optional_pair(&mut pairs, "source", config.source.as_deref());
    push_optional_pair(&mut pairs, "resolution", config.resolution.as_deref());
    push_optional_pair(&mut pairs, "release_group", config.release_group.as_deref());

    if let Some(anidb_id) = request_id(req, "anidb_id")
        .or_else(|| request_id(req, "anidb"))
        .filter(|value| !value.trim().is_empty())
    {
        pairs.push(("anime_id".to_string(), anidb_id));
    }
    pairs.extend(extra_pairs);

    encode_query_pairs(pairs)
}

fn provider_category_param(
    default_category: &Option<String>,
    req: &SearchRequest,
) -> Option<String> {
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
        Some(value) if value.eq_ignore_ascii_case("anime") => default_category
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        Some(_) => None,
        None => default_category
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    }
}

fn is_newznab_category_param(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn push_optional_pair(pairs: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        pairs.push((key.to_string(), value.to_string()));
    }
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

fn merge_query_params(left: &str, right: &str) -> String {
    match (left.trim(), right.trim()) {
        ("", "") => String::new(),
        ("", right) => normalize_additional_params(right),
        (left, "") => normalize_additional_params(left),
        (left, right) => format!(
            "{}{}",
            normalize_additional_params(left),
            normalize_additional_params(right)
        ),
    }
}

fn normalize_additional_params(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        String::new()
    } else if value.starts_with('&') {
        value.to_string()
    } else {
        format!("&{value}")
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
            | "releasegroup" | "translation" => {
                if !value.trim().is_empty() {
                    extra.insert(normalized, json!(value.trim()));
                }
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

#[cfg(not(test))]
fn config_optional(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
fn config_optional(_key: &str) -> Option<String> {
    None
}

#[cfg(not(test))]
fn config_usize_optional(key: &str) -> Option<usize> {
    config::get(key)
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse::<usize>().ok())
}

#[cfg(test)]
fn config_usize_optional(_key: &str) -> Option<usize> {
    None
}

#[cfg(not(test))]
fn config_bool(key: &str, default_value: bool) -> bool {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(default_value)
}

#[cfg(test)]
fn config_bool(_key: &str, default_value: bool) -> bool {
    default_value
}

#[cfg(not(test))]
fn config_u32(key: &str, default_value: u32) -> u32 {
    config::get(key)
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(default_value)
}

#[cfg(test)]
fn config_u32(_key: &str, default_value: u32) -> u32 {
    default_value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> SearchRequest {
        SearchRequest {
            query: "Frieren".to_string(),
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
            api_path: "/api".to_string(),
            additional_params: String::new(),
            page_size: DEFAULT_PAGE_SIZE,
            category: Some(DEFAULT_CATEGORY.to_string()),
            healthy_only: false,
            audio_lang: None,
            sub_lang: None,
            translation: None,
            source: None,
            resolution: None,
            release_group: None,
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
    }

    #[test]
    fn provider_params_include_anidb_category_and_filters_without_common_fields() {
        let mut config = ame_config();
        config.healthy_only = true;
        config.audio_lang = Some("jpn".to_string());
        config.sub_lang = Some("eng".to_string());
        config.translation = Some("subbed".to_string());
        config.source = Some("WEB".to_string());
        config.resolution = Some("1080p".to_string());
        config.release_group = Some("Group Name".to_string());

        let mut req = request();
        req.ids.insert("anidb".to_string(), "12345".to_string());

        let params = provider_params(&config, &req, Vec::new());

        assert!(params.starts_with('&'));
        assert!(params.contains("cat=5070"));
        assert!(params.contains("healthy=1"));
        assert!(params.contains("audio_lang=jpn"));
        assert!(params.contains("sub_lang=eng"));
        assert!(params.contains("translation=subbed"));
        assert!(params.contains("source=WEB"));
        assert!(params.contains("resolution=1080p"));
        assert!(params.contains("release_group=Group+Name"));
        assert!(params.contains("anime_id=12345"));
        assert!(!params.contains("season="));
        assert!(!params.contains("ep="));
    }

    #[test]
    fn provider_params_do_not_duplicate_category_when_request_has_categories() {
        let config = ame_config();
        let mut req = request();
        req.categories.push("2000".to_string());

        let params = provider_params(&config, &req, Vec::new());

        assert!(!params.contains("cat=5070"));
    }

    #[test]
    fn provider_params_respect_singular_category_hint() {
        let config = ame_config();

        let mut movie_req = request();
        movie_req.category = Some("movie".to_string());
        let movie_params = provider_params(&config, &movie_req, Vec::new());
        assert!(!movie_params.contains("cat=5070"));

        let mut numeric_req = request();
        numeric_req.category = Some("2000".to_string());
        let numeric_params = provider_params(&config, &numeric_req, Vec::new());
        assert!(numeric_params.contains("cat=2000"));

        let mut anime_req = request();
        anime_req.category = Some("anime".to_string());
        let anime_params = provider_params(&config, &anime_req, Vec::new());
        assert!(anime_params.contains("cat=5070"));
    }

    #[test]
    fn exact_hash_request_clears_broad_search_inputs_but_keeps_default_category() {
        let mut req = request();
        req.ids
            .insert("info_hash".to_string(), "ABCDEF".to_string());
        req.season = Some(1);
        req.episode = Some(2);

        let raw = req_for_exact_provider_filter(&req, &Some(DEFAULT_CATEGORY.to_string()));

        assert!(raw.query.is_empty());
        assert!(raw.ids.is_empty());
        assert_eq!(raw.categories, vec![DEFAULT_CATEGORY.to_string()]);
        assert_eq!(raw.season, None);
        assert_eq!(raw.episode, None);
    }

    #[test]
    fn direct_anidb_params_survive_raw_request_shape() {
        let config = ame_config();
        let mut req = request();
        req.ids.insert("anidb_id".to_string(), "12345".to_string());
        req.season = Some(2);
        req.absolute_episode = Some(12);

        let raw = req_for_exact_provider_filter(&req, &config.category);
        let params = provider_params(&config, &raw, anime_id_pairs(&req, "12345".to_string()));

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
    fn merge_query_params_keeps_user_and_provider_params() {
        assert_eq!(
            merge_query_params("dl=1", "&healthy=1&anime_id=12"),
            "&dl=1&healthy=1&anime_id=12"
        );
        assert_eq!(merge_query_params("", "healthy=1"), "&healthy=1");
        assert_eq!(merge_query_params("&dl=1", ""), "&dl=1");
    }

    #[test]
    fn effective_page_size_defaults_to_50_and_caps_at_api_max() {
        assert_eq!(effective_page_size(None), DEFAULT_PAGE_SIZE);
        assert_eq!(effective_page_size(Some(0)), 1);
        assert_eq!(effective_page_size(Some(75)), 75);
        assert_eq!(effective_page_size(Some(100)), MAX_PAGE_SIZE);
        assert_eq!(effective_page_size(Some(250)), MAX_PAGE_SIZE);
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
    fn http_behavior_is_cautious_and_budgeted_below_provider_daily_limit() {
        let mut config = NewznabConfig {
            base_url: AMENZB_BASE_URL.to_string(),
            api_key: String::new(),
            api_path: "/api".to_string(),
            additional_params: String::new(),
            page_size: DEFAULT_PAGE_SIZE,
            http_behavior: NewznabHttpBehavior::default(),
        };

        apply_amenzb_http_behavior(&mut config);

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
        assert_eq!(budget.daily_limit, DEFAULT_DAILY_HIT_CAP);
        assert!(budget.daily_limit < 10_000);
    }
}
