//! Shared Newznab protocol engine for indexer plugins.
//!
//! This crate provides the core Newznab/Torznab API client logic used by
//! both the generic `newznab` and the NZBGeek-specific `nzbgeek` plugins.
//! Each plugin is a thin wrapper that calls [`execute_full_search`] with
//! a provider-specific [`MetadataExtractor`] callback.

use std::collections::HashMap;

use extism_pdk::*;
use quick_xml::Reader;
use quick_xml::events::Event;
pub use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, IndexerCapabilities as Capabilities,
    IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor, IndexerFeedMode,
    IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures, IndexerSearchInput,
    IndexerSourceKind, IndexerTorrentCapabilities, PluginDescriptor, PluginResult,
    PluginScoringPolicy as ScoringPolicy, PluginSearchRequest as SearchRequest,
    PluginSearchResponse as SearchResponse, PluginSearchResult as SearchResult,
    PluginSearchSubjectKind, ProviderDescriptor, SDK_VERSION, current_sdk_constraint,
};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordMetadataClassification {
    Real(String),
    ProtectedFlag,
    UnprotectedFlag,
    Empty,
}

pub fn classify_password_metadata(raw: Option<&str>) -> PasswordMetadataClassification {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return PasswordMetadataClassification::Empty;
    };

    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "passworded" | "protected" => {
            PasswordMetadataClassification::ProtectedFlag
        }
        "0" | "false" | "no" => PasswordMetadataClassification::UnprotectedFlag,
        _ => PasswordMetadataClassification::Real(value.to_string()),
    }
}

fn classify_password_metadata_value(
    value: &serde_json::Value,
) -> Option<PasswordMetadataClassification> {
    if value.as_bool() == Some(true) {
        return Some(PasswordMetadataClassification::ProtectedFlag);
    }
    if value.as_bool() == Some(false) {
        return Some(PasswordMetadataClassification::UnprotectedFlag);
    }
    value
        .as_str()
        .map(|raw| classify_password_metadata(Some(raw)))
}

fn password_hint_from_metadata_value(value: &serde_json::Value) -> Option<String> {
    match classify_password_metadata_value(value)? {
        PasswordMetadataClassification::Real(password) => Some(password),
        PasswordMetadataClassification::ProtectedFlag
        | PasswordMetadataClassification::UnprotectedFlag
        | PasswordMetadataClassification::Empty => None,
    }
}

fn protection_hint_from_metadata_value(value: &serde_json::Value) -> Option<bool> {
    match classify_password_metadata_value(value)? {
        PasswordMetadataClassification::Real(_) | PasswordMetadataClassification::ProtectedFlag => {
            Some(true)
        }
        PasswordMetadataClassification::UnprotectedFlag => Some(false),
        PasswordMetadataClassification::Empty => None,
    }
}

fn password_hint_from_extra(extra: &HashMap<String, serde_json::Value>) -> Option<String> {
    extra
        .get("password")
        .and_then(password_hint_from_metadata_value)
}

fn protected_from_extra(extra: &HashMap<String, serde_json::Value>) -> Option<bool> {
    if let Some(value) = extra
        .get("password_protected")
        .and_then(|value| value.as_bool())
    {
        return Some(value);
    }

    extra
        .get("password")
        .and_then(protection_hint_from_metadata_value)
}

// ---------------------------------------------------------------------------
// Search request / response types
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

pub struct NewznabConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_path: String,
    pub additional_params: String,
    /// Maximum results the indexer returns per page. Defaults to 100.
    pub page_size: usize,
}

impl NewznabConfig {
    /// Read configuration from Extism host config keys.
    pub fn from_extism() -> Result<Self, Error> {
        let base_url = config::get("base_url")
            .map_err(|e| Error::msg(format!("missing config base_url: {e}")))?
            .unwrap_or_default()
            .trim()
            .to_string();
        let api_key = config::get("api_key")
            .map_err(|e| Error::msg(format!("missing config api_key: {e}")))?
            .unwrap_or_default()
            .trim()
            .to_string();
        let api_path = config::get("api_path")
            .ok()
            .flatten()
            .unwrap_or_else(|| "/api".to_string())
            .trim()
            .to_string();
        let additional_params = config::get("additional_params")
            .ok()
            .flatten()
            .unwrap_or_default()
            .trim()
            .to_string();

        if base_url.is_empty() {
            return Err(Error::msg(
                "Newznab indexer requires base_url configuration",
            ));
        }

        let page_size = config::get("page_size")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(100)
            .clamp(1, 100);
        Ok(Self {
            base_url,
            api_key,
            api_path,
            additional_params,
            page_size,
        })
    }
}

/// Returns the standard config field declarations for Newznab-family plugins.
pub fn standard_config_fields(default_base_url: Option<&str>) -> Vec<ConfigFieldDef> {
    vec![
        ConfigFieldDef {
            key: "base_url".to_string(),
            label: "Base URL".to_string(),
            field_type: ConfigFieldType::String,
            required: true,
            default_value: default_base_url.map(ToString::to_string),
            value_source: Default::default(),
            role: Some(ConfigFieldRole::ConnectionUrl),
            host_binding: None,
            options: vec![],
            help_text: Some("Indexer site URL, for example https://indexer.example".to_string()),
        },
        ConfigFieldDef {
            key: "api_key".to_string(),
            label: "API Key".to_string(),
            field_type: ConfigFieldType::Password,
            required: false,
            default_value: None,
            value_source: Default::default(),
            role: None,
            host_binding: None,
            options: vec![],
            help_text: Some("Indexer API key".to_string()),
        },
        ConfigFieldDef {
            key: "api_path".to_string(),
            label: "API Path".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: Some("/api".to_string()),
            value_source: Default::default(),
            role: None,
            host_binding: None,
            options: vec![],
            help_text: Some("API endpoint path (e.g. /api, /api/v1/api, /nabapi)".to_string()),
        },
        ConfigFieldDef {
            key: "additional_params".to_string(),
            label: "Additional Parameters".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: None,
            value_source: Default::default(),
            role: None,
            host_binding: None,
            options: vec![],
            help_text: Some(
                "Extra query parameters appended to every request (e.g. &dl=1&attrs=poster)"
                    .to_string(),
            ),
        },
    ]
}

// ---------------------------------------------------------------------------
// Metadata extraction callback
// ---------------------------------------------------------------------------

/// Callback type for provider-specific attribute extraction.
///
/// Given a slice of `(name, value)` pairs from Newznab `<attr>` elements,
/// returns `(languages, grabs, extra_map)`.
pub type MetadataExtractor =
    fn(&[(String, String)]) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>);

/// Default extractor for generic Newznab indexers: extracts common attributes.
pub fn extract_base_metadata(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut grabs = None;
    let mut languages = Vec::new();
    let mut extra = HashMap::new();

    for (name, value) in pairs {
        let normalized: String = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        match normalized.as_str() {
            "language" => {
                let items: Vec<String> = value
                    .split(" - ")
                    .map(|v| v.trim())
                    .filter(|v| !v.is_empty())
                    .map(ToString::to_string)
                    .collect();
                languages.extend(items);
            }
            "grabs" => {
                grabs = value.trim().replace(',', "").parse::<i64>().ok();
            }
            "password" => {
                match classify_password_metadata(Some(value)) {
                    PasswordMetadataClassification::Real(password) => {
                        extra.insert("password".to_string(), serde_json::Value::from(password));
                        extra.insert(
                            "password_protected".to_string(),
                            serde_json::Value::from(true),
                        );
                    }
                    PasswordMetadataClassification::ProtectedFlag => {
                        extra.insert(
                            "password_protected".to_string(),
                            serde_json::Value::from(true),
                        );
                    }
                    PasswordMetadataClassification::UnprotectedFlag => {
                        extra.insert(
                            "password_protected".to_string(),
                            serde_json::Value::from(false),
                        );
                    }
                    PasswordMetadataClassification::Empty => {}
                }
            }
            _ => {}
        }
    }

    (languages, grabs, extra)
}

// ---------------------------------------------------------------------------
// API limit metadata
// ---------------------------------------------------------------------------

/// Rate limit metadata returned by Newznab indexers in `<limits>` elements.
#[derive(Default)]
struct ApiLimits {
    api_current: Option<u32>,
    api_max: Option<u32>,
    grab_current: Option<u32>,
    grab_max: Option<u32>,
}

// ---------------------------------------------------------------------------
// Main search orchestrator
// ---------------------------------------------------------------------------

/// Execute a full Newznab search with tiered fallback.
///
/// This is the primary entry point for plugins. It:
/// 1. Validates inputs
/// 2. Determines search type (movie/tvsearch/search)
/// 3. Executes a tiered search (query+ID → ID-only → generic fallback)
/// 4. Auto-detects response format (JSON or XML) and parses
/// 5. Classifies errors with Newznab-specific handling
pub fn execute_full_search(
    config: &NewznabConfig,
    req: &SearchRequest,
    extract_fn: MetadataExtractor,
) -> Result<SearchResponse, Error> {
    let query = req.query.trim().to_string();
    let query_variants = build_query_variants(&query);

    let imdb_id = req
        .ids
        .get("imdb_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let tmdb_id = req
        .ids
        .get("tmdb_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let tvdb_id = req
        .ids
        .get("tvdb_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let tvrage_id = req
        .ids
        .get("tvrage_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let tvmaze_id = req
        .ids
        .get("tvmaze_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let anidb_id = req
        .ids
        .get("anidb_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let mal_id = req
        .ids
        .get("mal_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    if query.is_empty()
        && imdb_id.is_none()
        && tmdb_id.is_none()
        && tvdb_id.is_none()
        && tvrage_id.is_none()
        && tvmaze_id.is_none()
        && anidb_id.is_none()
        && mal_id.is_none()
    {
        return execute_rss_search(config, req, extract_fn);
    }

    // Determine search shape from typed context first, then legacy hints.
    let search_shape = determine_nab_search_shape(
        req,
        NabSearchShapeHints {
            categories: &req.categories,
            facet: req.facet.as_deref(),
            category: req.category.as_deref(),
            imdb_id: imdb_id.as_deref(),
            tvdb_id: tvdb_id.as_deref(),
            tvrage_id: tvrage_id.as_deref(),
            tvmaze_id: tvmaze_id.as_deref(),
        },
    );
    let search_type = search_shape.search_type();

    // Build Newznab category parameter (numeric codes only)
    let newznab_cat = build_category_param(&req.categories);

    let endpoint = build_endpoint(&config.base_url, &config.api_path)?;

    // Paginated search: fetch up to MAX_PAGES pages.
    // Stop early if a page returns fewer results than the page size.
    let page_size = config.page_size;
    const MAX_PAGES: usize = 30;
    let max_results = page_size * MAX_PAGES;
    let limit = if req.limit == 0 {
        max_results
    } else {
        req.limit.min(max_results)
    };

    let mut all_results: Vec<SearchResult> = Vec::new();
    let mut last_limits = ApiLimits::default();

    for page in 0..MAX_PAGES {
        let offset = page * page_size;
        let page_params = if config.additional_params.is_empty() {
            format!("&offset={offset}")
        } else {
            format!("{}&offset={offset}", config.additional_params)
        };

        let (status, body) = if search_shape == NabSearchShape::AnimeExact {
            execute_exact_anime_search(
                &endpoint,
                &query_variants,
                &config.api_key,
                tmdb_id.as_deref(),
                tvdb_id.as_deref(),
                tvrage_id.as_deref(),
                tvmaze_id.as_deref(),
                newznab_cat.as_deref(),
                page_size,
                req.season,
                req.episode,
                req.absolute_episode,
                &page_params,
            )?
        } else {
            execute_tiered_search(
                &endpoint,
                search_type,
                &query_variants,
                &config.api_key,
                imdb_id.as_deref(),
                tmdb_id.as_deref(),
                tvdb_id.as_deref(),
                tvrage_id.as_deref(),
                tvmaze_id.as_deref(),
                newznab_cat.as_deref(),
                page_size,
                req.season,
                req.episode,
                &page_params,
            )?
        };

        // Detect response format and check for errors
        let trimmed = body.trim_start();
        let is_xml = trimmed.starts_with("<?xml")
            || trimmed.starts_with("<rss")
            || trimmed.starts_with("<error");

        if is_xml {
            if let Some((code, description)) = parse_error_xml(&body) {
                if page == 0 {
                    return Err(classify_and_format_error(&code, &description));
                }
                break; // Later pages erroring is not fatal
            }
        } else if let Some((code, description)) = parse_error_json(&body) {
            if page == 0 {
                return Err(classify_and_format_error(&code, &description));
            }
            break;
        }

        if status >= 400 {
            if page == 0 {
                return Err(Error::msg(format!("Newznab API returned HTTP {status}")));
            }
            break;
        }

        let (page_results, limits) = if is_xml {
            parse_newznab_xml(&body, page_size, extract_fn)
        } else {
            parse_newznab_json(&body, page_size, extract_fn)
        };

        last_limits = limits;
        let page_count = page_results.len();
        all_results.extend(page_results);

        // Stop if this page was less than full (no more results)
        // or we've hit the overall max
        if page_count < page_size || all_results.len() >= max_results {
            break;
        }
    }

    // Respect the caller's requested limit
    if all_results.len() > limit {
        all_results.truncate(limit);
    }

    Ok(SearchResponse {
        results: all_results,
        api_current: last_limits.api_current,
        api_max: last_limits.api_max,
        grab_current: last_limits.grab_current,
        grab_max: last_limits.grab_max,
    })
}

// ---------------------------------------------------------------------------
// RSS / unfiltered API search
// ---------------------------------------------------------------------------

/// Fetch recent releases via unfiltered Newznab API calls (no query, no IDs).
///
/// Makes category-only requests using the appropriate search type per facet:
/// - Movie categories (2xxx) → `t=movie`
/// - TV/anime categories (5xxx) → `t=tvsearch`
/// - Unknown → both
fn execute_rss_search(
    config: &NewznabConfig,
    req: &SearchRequest,
    extract_fn: MetadataExtractor,
) -> Result<SearchResponse, Error> {
    let endpoint = build_endpoint(&config.base_url, &config.api_path)?;
    let newznab_cat = build_category_param(&req.categories);

    // If no categories provided, we can't make a meaningful RSS request
    let cat_str = match newznab_cat {
        Some(ref c) => c.as_str(),
        None => {
            log!(
                LogLevel::Debug,
                "rss_search: no categories provided, skipping"
            );
            return Ok(SearchResponse {
                results: vec![],
                api_current: None,
                api_max: None,
                grab_current: None,
                grab_max: None,
            });
        }
    };

    // Determine which search types to use based on categories and facet
    let has_movie_cats = req.categories.iter().any(|c| c.starts_with('2'));
    let has_tv_cats = req.categories.iter().any(|c| c.starts_with('5'));
    let facet_movie = matches!(req.facet.as_deref(), Some("movie"));
    let facet_tv = matches!(req.facet.as_deref(), Some("series" | "anime"));

    let mut search_types = Vec::new();
    if has_movie_cats || facet_movie {
        search_types.push("movie");
    }
    if has_tv_cats || facet_tv {
        search_types.push("tvsearch");
    }
    if search_types.is_empty() {
        // No clear signal — try both
        search_types.push("tvsearch");
        search_types.push("movie");
    }

    log!(
        LogLevel::Info,
        "rss_search: fetching recent releases cat={} search_types={:?}",
        cat_str,
        search_types
    );

    let mut all_results: Vec<SearchResult> = Vec::new();
    let mut last_limits = ApiLimits::default();

    for search_type in &search_types {
        let (status, body) = execute_search(
            &endpoint,
            search_type,
            None, // no query
            &config.api_key,
            None, // no imdb_id
            None, // no tmdb_id
            None, // no tvdb_id
            None, // no tvrage_id
            None, // no tvmaze_id
            Some(cat_str),
            config.page_size,
            None, // no season
            None, // no episode
            &config.additional_params,
        )?;

        let trimmed = body.trim_start();
        let is_xml = trimmed.starts_with("<?xml")
            || trimmed.starts_with("<rss")
            || trimmed.starts_with("<error");

        if is_xml {
            if let Some((code, description)) = parse_error_xml(&body) {
                log!(
                    LogLevel::Warn,
                    "RSS fetch error for t={}: {} — {}",
                    search_type,
                    code,
                    description
                );
                continue;
            }
        } else if let Some((code, description)) = parse_error_json(&body) {
            log!(
                LogLevel::Warn,
                "RSS fetch error for t={}: {} — {}",
                search_type,
                code,
                description
            );
            continue;
        }

        if status >= 400 {
            log!(
                LogLevel::Warn,
                "RSS fetch HTTP {} for t={}",
                status,
                search_type
            );
            continue;
        }

        let (page_results, limits) = if is_xml {
            parse_newznab_xml(&body, config.page_size, extract_fn)
        } else {
            parse_newznab_json(&body, config.page_size, extract_fn)
        };

        log!(
            LogLevel::Info,
            "rss_search: t={} returned {} results",
            search_type,
            page_results.len()
        );
        last_limits = limits;
        all_results.extend(page_results);
    }

    log!(
        LogLevel::Info,
        "rss_search: total {} results across {} search types",
        all_results.len(),
        search_types.len()
    );

    Ok(SearchResponse {
        results: all_results,
        api_current: last_limits.api_current,
        api_max: last_limits.api_max,
        grab_current: last_limits.grab_current,
        grab_max: last_limits.grab_max,
    })
}

// ---------------------------------------------------------------------------
// Search type determination
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NabSearchShape {
    Movie,
    Tv,
    AnimeExact,
    Generic,
}

impl NabSearchShape {
    fn search_type(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Tv | Self::AnimeExact => "tvsearch",
            Self::Generic => "search",
        }
    }
}

#[derive(Clone, Copy)]
struct NabSearchShapeHints<'a> {
    categories: &'a [String],
    facet: Option<&'a str>,
    category: Option<&'a str>,
    imdb_id: Option<&'a str>,
    tvdb_id: Option<&'a str>,
    tvrage_id: Option<&'a str>,
    tvmaze_id: Option<&'a str>,
}

fn determine_nab_search_shape(
    req: &SearchRequest,
    hints: NabSearchShapeHints<'_>,
) -> NabSearchShape {
    if let Some(context) = req.context.as_ref() {
        match context.subject_kind {
            PluginSearchSubjectKind::Movie => return NabSearchShape::Movie,
            PluginSearchSubjectKind::AnimeEpisode => return NabSearchShape::AnimeExact,
            PluginSearchSubjectKind::Episode
            | PluginSearchSubjectKind::Season
            | PluginSearchSubjectKind::Special => {
                if is_anime_request(hints.facet, hints.category) {
                    return NabSearchShape::AnimeExact;
                }
                return NabSearchShape::Tv;
            }
            PluginSearchSubjectKind::Title => {
                if matches!(hints.facet.map(str::trim), Some("movie")) {
                    return NabSearchShape::Movie;
                }
                if is_anime_request(hints.facet, hints.category) {
                    return NabSearchShape::AnimeExact;
                }
                if matches!(hints.facet.map(str::trim), Some("series")) {
                    return NabSearchShape::Tv;
                }
            }
            PluginSearchSubjectKind::Collection | PluginSearchSubjectKind::Unknown => {}
        }
    }

    if is_anime_request(hints.facet, hints.category) {
        return NabSearchShape::AnimeExact;
    }

    match determine_search_type(
        hints.categories,
        hints.facet,
        hints.category,
        hints.imdb_id,
        hints.tvdb_id,
        hints.tvrage_id,
        hints.tvmaze_id,
    )
    .as_str()
    {
        "movie" => NabSearchShape::Movie,
        "tvsearch" => NabSearchShape::Tv,
        _ => NabSearchShape::Generic,
    }
}

fn determine_search_type(
    categories: &[String],
    facet_hint: Option<&str>,
    category_hint: Option<&str>,
    imdb_id: Option<&str>,
    tvdb_id: Option<&str>,
    tvrage_id: Option<&str>,
    tvmaze_id: Option<&str>,
) -> String {
    let cats_movie = categories.iter().any(|c| c.starts_with('2'));
    let cats_tv = categories.iter().any(|c| c.starts_with('5'));

    let facet_movie = matches!(facet_hint.map(str::trim), Some("movie"));
    let facet_tv = matches!(facet_hint.map(str::trim), Some("series" | "anime"));
    let hint_movie = matches!(category_hint.map(str::trim), Some("movie"));
    let hint_tv = matches!(category_hint.map(str::trim), Some("series" | "anime"));

    if cats_movie {
        "movie".to_string()
    } else if cats_tv {
        "tvsearch".to_string()
    } else if facet_movie {
        "movie".to_string()
    } else if facet_tv {
        "tvsearch".to_string()
    } else if hint_movie {
        "movie".to_string()
    } else if hint_tv {
        "tvsearch".to_string()
    } else if imdb_id.is_some() {
        "movie".to_string()
    } else if tvdb_id.is_some() || tvrage_id.is_some() || tvmaze_id.is_some() {
        "tvsearch".to_string()
    } else {
        "search".to_string()
    }
}

fn build_category_param(categories: &[String]) -> Option<String> {
    let numeric_cats: Vec<&str> = categories
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_digit()))
        .collect();
    if numeric_cats.is_empty() {
        None
    } else {
        Some(numeric_cats.join(","))
    }
}

fn build_query_variants(query: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let query = strip_query_context(query);

    if !query.is_empty() {
        variants.push(query.to_string());
    }

    let mut seen = std::collections::HashSet::new();
    variants.retain(|value| seen.insert(value.to_ascii_lowercase()));
    variants
}

fn is_anime_request(facet_hint: Option<&str>, category_hint: Option<&str>) -> bool {
    matches!(facet_hint.map(str::trim), Some("anime"))
        || category_hint
            .map(|value| value.trim() == "anime")
            .unwrap_or(false)
}

fn looks_like_context_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        return false;
    }

    let upper = trimmed.to_ascii_uppercase();
    if upper == "OVA" || upper == "SPECIAL" {
        return true;
    }

    if let Some(rest) = upper.strip_prefix('S') {
        if rest.chars().all(|ch| ch.is_ascii_digit()) {
            return true;
        }
        if let Some((season_part, episode_part)) = rest.split_once('E') {
            return !season_part.is_empty()
                && !episode_part.is_empty()
                && season_part.chars().all(|ch| ch.is_ascii_digit())
                && episode_part.chars().all(|ch| ch.is_ascii_digit());
        }
    }

    trimmed.chars().all(|ch| ch.is_ascii_digit())
}

fn strip_query_context(query: &str) -> &str {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return query.trim();
    }

    let mut start = tokens.len();
    for index in (0..tokens.len()).rev() {
        if looks_like_context_token(tokens[index]) {
            start = index;
        } else if start != tokens.len() {
            break;
        }
    }

    if start == tokens.len() {
        query.trim()
    } else {
        query[..query.rfind(tokens[start]).unwrap_or(query.len())].trim()
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_exact_anime_search(
    endpoint: &str,
    query_variants: &[String],
    api_key: &str,
    tmdb_id: Option<&str>,
    tvdb_id: Option<&str>,
    tvrage_id: Option<&str>,
    tvmaze_id: Option<&str>,
    cat: Option<&str>,
    limit: usize,
    season: Option<u32>,
    episode: Option<u32>,
    absolute_episode: Option<u32>,
    additional_params: &str,
) -> Result<(u16, String), Error> {
    if tvdb_id.is_some() || tmdb_id.is_some() || tvrage_id.is_some() || tvmaze_id.is_some() {
        return execute_search(
            endpoint,
            "tvsearch",
            None,
            api_key,
            None,
            tmdb_id,
            tvdb_id,
            tvrage_id,
            tvmaze_id,
            cat,
            limit,
            if absolute_episode.is_some() {
                None
            } else {
                season
            },
            absolute_episode.or(episode),
            additional_params,
        );
    }

    let mut last_response = (200, r#"{"channel":{}}"#.to_string());
    for query_text in query_variants
        .iter()
        .map(String::as_str)
        .filter(|query| !query.is_empty())
    {
        let (status, body) = execute_search(
            endpoint,
            "tvsearch",
            Some(query_text),
            api_key,
            None,
            None,
            None,
            None,
            None,
            cat,
            limit,
            season,
            episode,
            additional_params,
        )?;
        let looks_empty = is_empty_response(body.trim_start());
        last_response = (status, body.clone());
        if is_success_status(status) && !looks_empty {
            return Ok((status, body));
        }
    }

    Ok(last_response)
}

// ---------------------------------------------------------------------------
// Tiered search execution
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_tiered_search(
    endpoint: &str,
    search_type: &str,
    query_variants: &[String],
    api_key: &str,
    imdb_id: Option<&str>,
    tmdb_id: Option<&str>,
    tvdb_id: Option<&str>,
    tvrage_id: Option<&str>,
    tvmaze_id: Option<&str>,
    cat: Option<&str>,
    limit: usize,
    season: Option<u32>,
    episode: Option<u32>,
    additional_params: &str,
) -> Result<(u16, String), Error> {
    // Determine effective IDs for the search type.
    let effective_imdb =
        if search_type == "movie" || search_type == "tvsearch" || search_type == "search" {
            imdb_id
        } else {
            None
        };
    let effective_tmdb =
        if search_type == "movie" || search_type == "tvsearch" || search_type == "search" {
            tmdb_id
        } else {
            None
        };
    let effective_tvdb = if search_type == "tvsearch" || search_type == "search" {
        tvdb_id
    } else {
        None
    };
    let effective_tvrage = if search_type == "tvsearch" || search_type == "search" {
        tvrage_id
    } else {
        None
    };
    let effective_tvmaze = if search_type == "tvsearch" || search_type == "search" {
        tvmaze_id
    } else {
        None
    };

    let has_id = effective_imdb.is_some()
        || effective_tmdb.is_some()
        || effective_tvdb.is_some()
        || effective_tvrage.is_some()
        || effective_tvmaze.is_some();
    let mut last_response: Option<(u16, String)> = None;

    // Tier 1: ID-only search when we have authoritative IDs. Do not mix q with
    // IDs; some nab providers treat that as a narrower text search and return
    // stale/low-quality matches before the authoritative ID lane is tried.
    if has_id {
        let (status, body) = execute_search(
            endpoint,
            search_type,
            None,
            api_key,
            effective_imdb,
            effective_tmdb,
            effective_tvdb,
            effective_tvrage,
            effective_tvmaze,
            cat,
            limit,
            season,
            episode,
            additional_params,
        )?;

        let looks_empty = is_empty_response(body.trim_start());
        last_response = Some((status, body.clone()));
        if is_success_status(status) && !looks_empty {
            return Ok((status, body));
        }
    }

    // Tier 2: focused text fallback without IDs.
    for query_text in query_variants
        .iter()
        .map(String::as_str)
        .filter(|query| !query.is_empty())
    {
        let (status, body) = execute_search(
            endpoint,
            search_type,
            Some(query_text),
            api_key,
            None,
            None,
            None,
            None,
            None,
            cat,
            limit,
            season,
            episode,
            additional_params,
        )?;

        let looks_empty = is_empty_response(body.trim_start());
        last_response = Some((status, body.clone()));
        if is_success_status(status) && !looks_empty {
            return Ok((status, body));
        }
    }

    Ok(last_response.unwrap_or((200, r#"{"channel":{}}"#.to_string())))
}

fn is_success_status(status: u16) -> bool {
    (200..300).contains(&status)
}

fn is_empty_response(trimmed: &str) -> bool {
    // JSON: no items
    if trimmed.starts_with('{') {
        return !trimmed.contains("\"title\"");
    }
    // XML: no <item> elements
    if trimmed.starts_with('<') {
        return !trimmed.contains("<item");
    }
    true
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn build_endpoint(base_url: &str, api_path: &str) -> Result<String, Error> {
    let trimmed_base = base_url.trim();
    let mut parsed = Url::parse(trimmed_base)
        .map_err(|error| Error::msg(format!("invalid base_url: {error}")))?;
    let base_path = parsed.path().trim_end_matches('/').to_string();
    let normalized_path = api_path.trim().trim_matches('/');
    let next_path = if normalized_path.is_empty() {
        base_path
    } else if base_path.is_empty() || base_path == "/" {
        format!("/{normalized_path}")
    } else {
        format!("{base_path}/{normalized_path}")
    };

    if next_path.is_empty() {
        parsed.set_path("/");
    } else {
        parsed.set_path(&next_path);
    }
    parsed.set_query(None);
    parsed.set_fragment(None);

    let mut rendered = parsed.to_string();
    if next_path.is_empty() && parsed.path() == "/" && !trimmed_base.ends_with('/') {
        rendered = rendered.trim_end_matches('/').to_string();
    }

    Ok(rendered)
}

#[allow(clippy::too_many_arguments)]
fn execute_search(
    endpoint: &str,
    search_type: &str,
    query: Option<&str>,
    api_key: &str,
    imdb_id: Option<&str>,
    tmdb_id: Option<&str>,
    tvdb_id: Option<&str>,
    tvrage_id: Option<&str>,
    tvmaze_id: Option<&str>,
    cat: Option<&str>,
    limit: usize,
    season: Option<u32>,
    episode: Option<u32>,
    additional_params: &str,
) -> Result<(u16, String), Error> {
    let url = build_search_url(
        endpoint,
        search_type,
        query,
        api_key,
        imdb_id,
        tmdb_id,
        tvdb_id,
        tvrage_id,
        tvmaze_id,
        cat,
        limit,
        season,
        episode,
        additional_params,
    )?;

    let (status, body) = http_get_with_retry(&url)?;
    Ok((status, body))
}

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// HTTP GET with 429 retry handling.
///
/// On a 429 response, retries with escalating backoff: 2s → 5s → 10s.
/// Respects `Retry-After` / `X-Retry-After` headers if present.
/// If the 429 persists after 10s (or Retry-After > 10s), returns an error.
fn http_get_with_retry(url: &str) -> Result<(u16, String), Error> {
    const BACKOFF_SECS: &[u64] = &[2, 5, 10];

    let logged_url = redact_url_for_log(url);

    let http_req = HttpRequest::new(url)
        .with_header("Accept", "application/json, application/xml, */*; q=0.8")
        .with_header("Accept-Encoding", "gzip")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("User-Agent", USER_AGENT);

    let mut next_delay: u64 = 0;
    for (attempt, fallback_delay) in BACKOFF_SECS
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None))
        .enumerate()
    {
        if next_delay > 0 {
            let start = std::time::Instant::now();
            let wait = std::time::Duration::from_secs(next_delay);
            while start.elapsed() < wait {
                std::hint::spin_loop();
            }
        }

        log!(
            LogLevel::Debug,
            "http_trace plugin=newznab method=GET attempt={} url={}",
            attempt + 1,
            logged_url
        );

        let resp = http::request::<Vec<u8>>(&http_req, None).map_err(|e| {
            log!(
                LogLevel::Debug,
                "http_trace_error plugin=newznab method=GET attempt={} url={} error={}",
                attempt + 1,
                logged_url,
                e
            );
            Error::msg(format!("HTTP request failed: {e}"))
        })?;

        log!(
            LogLevel::Debug,
            "http_trace_response plugin=newznab method=GET attempt={} status={} url={}",
            attempt + 1,
            resp.status_code(),
            logged_url
        );

        if resp.status_code() == 429 {
            let Some(fallback_delay) = fallback_delay else {
                return Err(Error::msg("HTTP 429: rate limited after all retries"));
            };

            let server_delay = resp
                .headers()
                .get("retry-after")
                .or_else(|| resp.headers().get("x-retry-after"))
                .and_then(|v| v.parse::<u64>().ok());

            next_delay = match server_delay {
                Some(secs) if secs > 10 => {
                    return Err(Error::msg(format!(
                        "HTTP 429: Retry-After {secs}s exceeds maximum"
                    )));
                }
                Some(secs) => secs,
                None => fallback_delay,
            };
            continue;
        }

        return Ok((
            resp.status_code(),
            String::from_utf8_lossy(&resp.body()).to_string(),
        ));
    }

    Err(Error::msg("HTTP request exhausted all retries"))
}

fn redact_url_for_log(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };

    let redacted_query = query
        .split('&')
        .map(|pair| {
            let Some((key, value)) = pair.split_once('=') else {
                return pair.to_string();
            };

            if is_sensitive_query_key(key) {
                format!("{key}=REDACTED")
            } else {
                format!("{key}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&");

    format!("{base}?{redacted_query}")
}

fn is_sensitive_query_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "apikey" | "api_key" | "token" | "key" | "password" | "pass"
    )
}

#[allow(clippy::too_many_arguments)]
fn build_search_url(
    endpoint: &str,
    search_type: &str,
    query: Option<&str>,
    api_key: &str,
    imdb_id: Option<&str>,
    tmdb_id: Option<&str>,
    tvdb_id: Option<&str>,
    tvrage_id: Option<&str>,
    tvmaze_id: Option<&str>,
    cat: Option<&str>,
    limit: usize,
    season: Option<u32>,
    episode: Option<u32>,
    additional_params: &str,
) -> Result<String, Error> {
    let imdb_id = imdb_id.map(normalize_imdbid_param);
    let mut url =
        Url::parse(endpoint).map_err(|error| Error::msg(format!("invalid endpoint: {error}")))?;

    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("t", search_type);
        if !api_key.trim().is_empty() {
            pairs.append_pair("apikey", api_key.trim());
        }
        pairs.append_pair("extended", "1");
        pairs.append_pair("limit", &limit.to_string());

        if let Some(q) = query.map(str::trim).filter(|value| !value.is_empty()) {
            pairs.append_pair("q", q);
        }
        if let Some(id) = imdb_id.as_deref() {
            pairs.append_pair("imdbid", id);
        }
        if let Some(id) = tmdb_id.map(str::trim).filter(|value| !value.is_empty()) {
            pairs.append_pair("tmdbid", id);
        }
        if let Some(id) = tvdb_id.map(str::trim).filter(|value| !value.is_empty()) {
            pairs.append_pair("tvdbid", id);
        }
        if let Some(id) = tvrage_id.map(str::trim).filter(|value| !value.is_empty()) {
            pairs.append_pair("rid", id);
        }
        if let Some(id) = tvmaze_id.map(str::trim).filter(|value| !value.is_empty()) {
            pairs.append_pair("tvmazeid", id);
        }
        if let Some(c) = cat.map(str::trim).filter(|value| !value.is_empty()) {
            pairs.append_pair("cat", c);
        }
        if let Some(s) = season {
            pairs.append_pair("season", &s.to_string());
        }
        if let Some(e) = episode {
            pairs.append_pair("ep", &e.to_string());
        }
    }

    append_additional_query_pairs(&mut url, additional_params);

    Ok(url.to_string())
}

fn append_additional_query_pairs(url: &mut Url, additional_params: &str) {
    let normalized = additional_params
        .trim()
        .trim_start_matches('?')
        .trim_start_matches('&');
    if normalized.is_empty() {
        return;
    }

    let mut pairs = url.query_pairs_mut();
    for (raw_key, raw_value) in url::form_urlencoded::parse(normalized.as_bytes()) {
        let key = raw_key.trim();
        if key.is_empty() {
            continue;
        }

        pairs.append_pair(key, raw_value.trim());
    }
}

fn normalize_imdbid_param(raw: &str) -> String {
    normalize_imdbid_param_with_mode(raw, use_canonical_imdb_ids())
}

fn normalize_imdbid_param_with_mode(raw: &str, canonical: bool) -> String {
    let trimmed = raw.trim();
    if canonical {
        if trimmed.len() > 2
            && trimmed[..2].eq_ignore_ascii_case("tt")
            && trimmed[2..].chars().all(|ch| ch.is_ascii_digit())
        {
            format!("tt{}", &trimmed[2..])
        } else if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
            format!("tt{trimmed}")
        } else {
            trimmed.to_string()
        }
    } else if trimmed.len() > 2
        && trimmed[..2].eq_ignore_ascii_case("tt")
        && trimmed[2..].chars().all(|ch| ch.is_ascii_digit())
    {
        format!("00{}", &trimmed[2..])
    } else {
        trimmed.to_string()
    }
}

fn use_canonical_imdb_ids() -> bool {
    config::get("imdb_id_format")
        .ok()
        .flatten()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "canonical" | "tt"
            )
        })
}

#[cfg(test)]
/// Minimal percent-encoding for query string values.
fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char)
            }
            b' ' => output.push_str("%20"),
            _ => {
                output.push('%');
                output.push_str(&format!("{byte:02X}"));
            }
        }
    }
    output
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

fn classify_and_format_error(code: &str, description: &str) -> Error {
    let code_num: u32 = code.parse().unwrap_or(0);
    let prefix = if (100..200).contains(&code_num) {
        "Newznab API key error"
    } else if description.to_ascii_lowercase().contains("request limit") {
        "Newznab rate limit"
    } else {
        "Newznab error"
    };
    Error::msg(format!("{prefix} {code}: {description}"))
}

// ---------------------------------------------------------------------------
// Standard Newznab attribute extraction (used by both JSON and XML parsers)
// ---------------------------------------------------------------------------

/// Extract standard Newznab attributes that are always captured regardless
/// of the provider-specific MetadataExtractor. These are applied AFTER the
/// custom extractor runs.
fn apply_standard_attrs(
    pairs: &[(String, String)],
    result: &mut SearchResult,
    usenet_date: &mut Option<String>,
) {
    for (name, value) in pairs {
        let normalized: String = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        match normalized.as_str() {
            "usenetdate" => {
                *usenet_date = Some(value.clone());
            }
            "tvdbid" if !value.is_empty() && value != "0" => {
                result.provider_extra.insert(
                    "response_tvdbid".to_string(),
                    serde_json::Value::from(value.as_str()),
                );
            }
            "imdb" | "imdbid" if !value.is_empty() && value != "0" => {
                result.provider_extra.insert(
                    "response_imdbid".to_string(),
                    serde_json::Value::from(value.as_str()),
                );
            }
            "prematch" | "haspretime" if value != "0" => {
                let flags = result
                    .provider_extra
                    .entry("indexer_flags".to_string())
                    .or_insert_with(|| serde_json::Value::Array(vec![]));
                if let serde_json::Value::Array(ref mut arr) = flags {
                    arr.push(serde_json::Value::from("scene"));
                }
            }
            "nuked" if value != "0" => {
                let flags = result
                    .provider_extra
                    .entry("indexer_flags".to_string())
                    .or_insert_with(|| serde_json::Value::Array(vec![]));
                if let serde_json::Value::Array(ref mut arr) = flags {
                    arr.push(serde_json::Value::from("nuked"));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// JSON error parsing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NewznabJsonResponse {
    channel: Option<NewznabJsonChannel>,
    error: Option<NewznabJsonErrorNode>,
    #[serde(default)]
    limits: Option<NewznabJsonLimitsNode>,
}

#[derive(Deserialize)]
struct NewznabJsonLimitsNode {
    #[serde(rename = "@attributes", default)]
    attributes: Option<NewznabJsonLimitsAttrs>,
}

#[derive(Deserialize, Default)]
struct NewznabJsonLimitsAttrs {
    #[serde(default)]
    api_current: Option<String>,
    #[serde(default)]
    api_max: Option<String>,
    #[serde(default)]
    grab_current: Option<String>,
    #[serde(default)]
    grab_max: Option<String>,
}

#[derive(Deserialize)]
struct NewznabJsonChannel {
    item: Option<NewznabJsonItems>,
    account: Option<NewznabJsonAccountNode>,
}

#[derive(Deserialize)]
struct NewznabJsonAccountNode {
    #[serde(rename = "@attributes")]
    attributes: Option<NewznabJsonAccountAttrs>,
}

#[derive(Deserialize)]
struct NewznabJsonAccountAttrs {
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NewznabJsonItems {
    Many(Vec<NewznabJsonItem>),
    One(Box<NewznabJsonItem>),
}

impl NewznabJsonItems {
    fn into_vec(self) -> Vec<NewznabJsonItem> {
        match self {
            NewznabJsonItems::Many(v) => v,
            NewznabJsonItems::One(v) => vec![*v],
        }
    }
}

#[derive(Deserialize)]
struct NewznabJsonItem {
    title: Option<String>,
    guid: Option<String>,
    link: Option<String>,
    comments: Option<String>,
    #[serde(rename = "pubDate")]
    pub_date: Option<String>,
    enclosure: Option<NewznabJsonEnclosure>,
    attr: Option<NewznabJsonAttributes>,
}

#[derive(Deserialize)]
struct NewznabJsonEnclosure {
    #[serde(rename = "@attributes")]
    attributes: Option<NewznabJsonEnclosureAttrs>,
}

#[derive(Deserialize)]
struct NewznabJsonEnclosureAttrs {
    url: Option<String>,
    length: Option<String>,
    #[serde(rename = "type")]
    mime_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NewznabJsonAttributes {
    Many(Vec<NewznabJsonAttributeNode>),
    One(Box<NewznabJsonAttributeNode>),
}

impl NewznabJsonAttributes {
    fn into_vec(self) -> Vec<NewznabJsonAttributeNode> {
        match self {
            NewznabJsonAttributes::Many(v) => v,
            NewznabJsonAttributes::One(v) => vec![*v],
        }
    }
}

#[derive(Deserialize)]
struct NewznabJsonAttributeNode {
    #[serde(rename = "@attributes")]
    attributes: Option<NewznabJsonAttributeAttrs>,
}

#[derive(Deserialize)]
struct NewznabJsonAttributeAttrs {
    name: Option<String>,
    value: Option<String>,
}

#[derive(Deserialize)]
struct NewznabJsonErrorNode {
    #[serde(rename = "@attributes")]
    attributes: Option<NewznabJsonErrorAttrs>,
}

#[derive(Deserialize)]
struct NewznabJsonErrorAttrs {
    code: Option<String>,
    description: Option<String>,
}

fn parse_error_json(body: &str) -> Option<(String, String)> {
    let parsed: NewznabJsonResponse = serde_json::from_str(body).ok()?;
    if let Some(attrs) = parsed.error.and_then(|error| error.attributes) {
        let code = attrs.code.unwrap_or_else(|| "unknown".into());
        let description = attrs.description.unwrap_or_else(|| "unknown".into());
        return Some((code, description));
    }

    let status = parsed
        .channel
        .and_then(|channel| channel.account)
        .and_then(|account| account.attributes)
        .and_then(|attrs| attrs.status)?;
    error_from_account_status(&status)
}

fn error_from_account_status(status: &str) -> Option<(String, String)> {
    let status = status.trim();
    if status.is_empty() {
        return None;
    }

    let lower = status.to_ascii_lowercase();
    if lower.contains("invalid") && lower.contains("key") {
        Some(("100".to_string(), status.to_string()))
    } else if lower.contains("error") || lower.contains("denied") || lower.contains("disabled") {
        Some(("unknown".to_string(), status.to_string()))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// JSON response parsing
// ---------------------------------------------------------------------------

fn parse_newznab_json(
    body: &str,
    limit: usize,
    extract_fn: MetadataExtractor,
) -> (Vec<SearchResult>, ApiLimits) {
    let parsed: NewznabJsonResponse = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (vec![], ApiLimits::default()),
    };

    // Extract API limits from channel.limits if present
    let limits = parsed
        .limits
        .map(|l| {
            let attrs = l.attributes.unwrap_or_default();
            ApiLimits {
                api_current: attrs.api_current.and_then(|v| v.parse().ok()),
                api_max: attrs.api_max.and_then(|v| v.parse().ok()),
                grab_current: attrs.grab_current.and_then(|v| v.parse().ok()),
                grab_max: attrs.grab_max.and_then(|v| v.parse().ok()),
            }
        })
        .unwrap_or_default();

    let items = match parsed.channel.and_then(|c| c.item) {
        Some(items) => items.into_vec(),
        None => return (vec![], limits),
    };

    let results: Vec<SearchResult> = items
        .into_iter()
        .take(limit)
        .filter_map(|item| {
            let title = item.title?;
            let enclosure_attrs = item.enclosure.and_then(|e| e.attributes);
            let download_url = enclosure_attrs.as_ref().and_then(|a| a.url.clone());
            let size_bytes = enclosure_attrs
                .as_ref()
                .and_then(|a| a.length.as_ref())
                .and_then(|v| v.replace(',', "").parse::<i64>().ok());

            // Check enclosure MIME type
            let enclosure_type = enclosure_attrs.as_ref().and_then(|a| a.mime_type.clone());

            // Extract attr pairs
            let pairs: Vec<(String, String)> = item
                .attr
                .map(|a| {
                    a.into_vec()
                        .into_iter()
                        .filter_map(|node| {
                            let attrs = node.attributes?;
                            Some((attrs.name?, attrs.value?))
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Run provider-specific extractor
            let (languages, grabs, extra) = extract_fn(&pairs);

            let mut result = SearchResult {
                title,
                link: item.link,
                download_url,
                size_bytes,
                published_at: item.pub_date.clone(),
                grabs,
                languages,
                thumbs_up: extra
                    .get("thumbs_up")
                    .and_then(|value| value.as_i64())
                    .map(|value| value as i32),
                thumbs_down: extra
                    .get("thumbs_down")
                    .and_then(|value| value.as_i64())
                    .map(|value| value as i32),
                subtitles: extra
                    .get("subtitles")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_default(),
                password_hint: password_hint_from_extra(&extra),
                protected: protected_from_extra(&extra),
                provider_extra: extra,
                guid: item.guid,
                info_url: item
                    .comments
                    .as_ref()
                    .map(|c| c.split('#').next().unwrap_or(c).trim().to_string())
                    .filter(|s| !s.is_empty()),
                ..SearchResult::default()
            };

            // Apply standard attrs (usenetdate, prematch, nuked, response IDs)
            let mut usenet_date = None;
            apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
            apply_provider_extra_fields(&mut result);

            // Prefer usenetdate over pubDate
            if usenet_date.is_some() {
                result.published_at = usenet_date;
            }

            // Store non-NZB enclosure type as metadata
            if let Some(ref mime) = enclosure_type {
                if mime != "application/x-nzb" {
                    result.provider_extra.insert(
                        "enclosure_type".to_string(),
                        serde_json::Value::from(mime.as_str()),
                    );
                }
            }

            Some(result)
        })
        .collect();

    (results, limits)
}

// ---------------------------------------------------------------------------
// XML error parsing
// ---------------------------------------------------------------------------

fn parse_error_xml(body: &str) -> Option<(String, String)> {
    let mut reader = Reader::from_str(body);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) if e.name().as_ref() == b"error" => {
                let mut code = None;
                let mut description = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"code" => {
                            code = String::from_utf8(attr.value.to_vec()).ok();
                        }
                        b"description" => {
                            description = String::from_utf8(attr.value.to_vec()).ok();
                        }
                        _ => {}
                    }
                }
                if code.is_some() || description.is_some() {
                    return Some((
                        code.unwrap_or_else(|| "unknown".into()),
                        description.unwrap_or_else(|| "unknown".into()),
                    ));
                }
            }
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                if e.name().as_ref() == b"account" =>
            {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"status" {
                        if let Ok(status) = String::from_utf8(attr.value.to_vec()) {
                            if let Some(error) = error_from_account_status(&status) {
                                return Some(error);
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

fn apply_provider_extra_fields(result: &mut SearchResult) {
    let seeders = extra_i64(&result.provider_extra, "seeders");
    let peers = extra_i64(&result.provider_extra, "peers");
    let leechers = extra_i64(&result.provider_extra, "leechers").or_else(|| {
        seeders
            .zip(peers)
            .and_then(|(seeders, peers)| peers.checked_sub(seeders))
    });

    if result.seeders.is_none() {
        result.seeders = seeders;
    }
    if result.peers.is_none() {
        result.peers = peers;
    }
    if result.leechers.is_none() {
        result.leechers = leechers;
    }
    if result.download_volume_factor.is_none() {
        result.download_volume_factor = extra_f64(&result.provider_extra, "downloadvolumefactor");
    }
    if result.upload_volume_factor.is_none() {
        result.upload_volume_factor = extra_f64(&result.provider_extra, "uploadvolumefactor");
    }
    if result.minimum_seed_ratio.is_none() {
        result.minimum_seed_ratio = extra_f64(&result.provider_extra, "minimumratio");
    }
    if result.minimum_seed_time_minutes.is_none() {
        result.minimum_seed_time_minutes = extra_i64(&result.provider_extra, "minimumseedtime");
    }
    if result.info_hash_v1.is_none() {
        result.info_hash_v1 = extra_string(&result.provider_extra, "info_hash");
    }
    if result.magnet_url.is_none() {
        result.magnet_url = extra_string(&result.provider_extra, "magnet_uri");
    }

    let mut flags = result.indexer_flags.clone();
    extend_flags(
        &mut flags,
        extra_string_array(&result.provider_extra, "indexer_flags"),
    );

    if result
        .provider_extra
        .get("freeleech")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        push_flag(&mut flags, "freeleech");
    }

    if let Some(download_factor) = result.download_volume_factor {
        if (download_factor - 0.5).abs() < f64::EPSILON {
            push_flag(&mut flags, "halfleech");
        } else if (download_factor - 0.75).abs() < f64::EPSILON {
            push_flag(&mut flags, "freeleech25");
        } else if (download_factor - 0.25).abs() < f64::EPSILON {
            push_flag(&mut flags, "freeleech75");
        } else if (download_factor - 0.0).abs() < f64::EPSILON {
            push_flag(&mut flags, "freeleech");
        }
    }

    if result
        .upload_volume_factor
        .is_some_and(|value| (value - 2.0).abs() < f64::EPSILON)
    {
        push_flag(&mut flags, "doubleupload");
    }

    for tag in extra_string_array(&result.provider_extra, "tags") {
        match tag.trim().to_ascii_lowercase().as_str() {
            "internal" => push_flag(&mut flags, "internal"),
            "scene" => push_flag(&mut flags, "scene"),
            _ => {}
        }
    }

    result.indexer_flags = flags;
}

fn extra_i64(extra: &HashMap<String, serde_json::Value>, key: &str) -> Option<i64> {
    extra.get(key).and_then(|value| value.as_i64())
}

fn extra_f64(extra: &HashMap<String, serde_json::Value>, key: &str) -> Option<f64> {
    extra.get(key).and_then(|value| value.as_f64())
}

fn extra_string(extra: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    extra
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn extra_string_array(extra: &HashMap<String, serde_json::Value>, key: &str) -> Vec<String> {
    extra
        .get(key)
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn extend_flags(flags: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        push_flag(flags, &value);
    }
}

fn push_flag(flags: &mut Vec<String>, value: &str) {
    let normalized = value.trim().to_ascii_lowercase();
    if !normalized.is_empty() && !flags.iter().any(|existing| existing == &normalized) {
        flags.push(normalized);
    }
}

// ---------------------------------------------------------------------------
// XML (RSS) response parsing
// ---------------------------------------------------------------------------

fn parse_newznab_xml(
    body: &str,
    limit: usize,
    extract_fn: MetadataExtractor,
) -> (Vec<SearchResult>, ApiLimits) {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut results = Vec::new();
    let mut api_limits = ApiLimits::default();
    let mut in_item = false;

    // Per-item accumulators
    let mut title: Option<String> = None;
    let mut guid: Option<String> = None;
    let mut link: Option<String> = None;
    let mut comments: Option<String> = None;
    let mut pub_date: Option<String> = None;
    let mut download_url: Option<String> = None;
    let mut size_bytes: Option<i64> = None;
    let mut enclosure_type: Option<String> = None;
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut current_tag: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag_name == "item" {
                    in_item = true;
                    title = None;
                    guid = None;
                    link = None;
                    comments = None;
                    pub_date = None;
                    download_url = None;
                    size_bytes = None;
                    enclosure_type = None;
                    attrs.clear();
                    current_tag = None;
                } else if in_item {
                    match tag_name.as_str() {
                        "title" | "guid" | "link" | "comments" | "pubDate" => {
                            current_tag = Some(tag_name);
                        }
                        "enclosure" => {
                            parse_enclosure_attrs(
                                e,
                                &mut download_url,
                                &mut size_bytes,
                                &mut enclosure_type,
                            );
                        }
                        _ => {
                            current_tag = None;
                        }
                    }
                }
            }
            Ok(Event::Empty(ref e)) if !in_item => {
                // Parse <limits> or <newznab:limits> at channel level
                let name_bytes = e.name().as_ref().to_vec();
                let local_name = String::from_utf8_lossy(&name_bytes);
                if local_name == "limits" || local_name.ends_with(":limits") {
                    for a in e.attributes().flatten() {
                        match a.key.as_ref() {
                            b"api_current" => {
                                api_limits.api_current = String::from_utf8(a.value.to_vec())
                                    .ok()
                                    .and_then(|v| v.parse().ok());
                            }
                            b"api_max" => {
                                api_limits.api_max = String::from_utf8(a.value.to_vec())
                                    .ok()
                                    .and_then(|v| v.parse().ok());
                            }
                            b"grab_current" => {
                                api_limits.grab_current = String::from_utf8(a.value.to_vec())
                                    .ok()
                                    .and_then(|v| v.parse().ok());
                            }
                            b"grab_max" => {
                                api_limits.grab_max = String::from_utf8(a.value.to_vec())
                                    .ok()
                                    .and_then(|v| v.parse().ok());
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(Event::Empty(ref e)) if in_item => {
                let qname = e.name();
                let local_name = String::from_utf8_lossy(qname.as_ref());
                let is_attr = local_name == "attr" || local_name.ends_with(":attr");
                if is_attr {
                    let mut attr_name = None;
                    let mut attr_value = None;
                    for a in e.attributes().flatten() {
                        match a.key.as_ref() {
                            b"name" => {
                                attr_name = String::from_utf8(a.value.to_vec()).ok();
                            }
                            b"value" => {
                                attr_value = String::from_utf8(a.value.to_vec()).ok();
                            }
                            _ => {}
                        }
                    }
                    if let (Some(n), Some(v)) = (attr_name, attr_value) {
                        attrs.push((n, v));
                    }
                } else if qname.as_ref() == b"enclosure" {
                    parse_enclosure_attrs(
                        e,
                        &mut download_url,
                        &mut size_bytes,
                        &mut enclosure_type,
                    );
                }
            }
            Ok(Event::Text(ref e)) if in_item => {
                if let Some(ref tag) = current_tag {
                    let text = e.unescape().map(|s| s.to_string()).unwrap_or_default();
                    match tag.as_str() {
                        "title" => title = Some(text),
                        "guid" => guid = Some(text),
                        "link" => link = Some(text),
                        "comments" => comments = Some(text),
                        "pubDate" => pub_date = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag_name == "item" && in_item {
                    in_item = false;
                    if let Some(ref t) = title {
                        if !t.is_empty() {
                            // Fallback: use size from newznab:attr if enclosure length was 0 or missing
                            if size_bytes.is_none() || size_bytes == Some(0) {
                                for (n, v) in &attrs {
                                    if n == "size" {
                                        size_bytes = v.replace(',', "").parse::<i64>().ok();
                                        break;
                                    }
                                }
                            }

                            // Run provider-specific extractor
                            let (languages, grabs, extra) = extract_fn(&attrs);

                            let info_url = comments
                                .as_ref()
                                .map(|c| c.split('#').next().unwrap_or(c).trim().to_string())
                                .filter(|s| !s.is_empty());

                            let mut result = SearchResult {
                                title: t.clone(),
                                link: link.clone(),
                                download_url: download_url.clone(),
                                size_bytes,
                                published_at: pub_date.clone(),
                                grabs,
                                languages,
                                thumbs_up: extra
                                    .get("thumbs_up")
                                    .and_then(|value| value.as_i64())
                                    .map(|value| value as i32),
                                thumbs_down: extra
                                    .get("thumbs_down")
                                    .and_then(|value| value.as_i64())
                                    .map(|value| value as i32),
                                subtitles: extra
                                    .get("subtitles")
                                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                                    .unwrap_or_default(),
                                password_hint: password_hint_from_extra(&extra),
                                protected: protected_from_extra(&extra),
                                provider_extra: extra,
                                guid: guid.clone(),
                                info_url,
                                ..SearchResult::default()
                            };

                            // Apply standard attrs
                            let mut usenet_date = None;
                            apply_standard_attrs(&attrs, &mut result, &mut usenet_date);
                            apply_provider_extra_fields(&mut result);

                            if usenet_date.is_some() {
                                result.published_at = usenet_date;
                            }

                            // Store non-NZB enclosure type
                            if let Some(ref mime) = enclosure_type {
                                if mime != "application/x-nzb" {
                                    result.provider_extra.insert(
                                        "enclosure_type".to_string(),
                                        serde_json::Value::from(mime.as_str()),
                                    );
                                }
                            }

                            results.push(result);

                            if results.len() >= limit {
                                break;
                            }
                        }
                    }
                }
                current_tag = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    (results, api_limits)
}

fn parse_enclosure_attrs(
    e: &quick_xml::events::BytesStart<'_>,
    download_url: &mut Option<String>,
    size_bytes: &mut Option<i64>,
    enclosure_type: &mut Option<String>,
) {
    let mut candidate_url = None;
    let mut candidate_size = None;
    let mut candidate_type = None;

    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"url" => {
                candidate_url = attr.unescape_value().ok().map(|value| value.to_string());
            }
            b"length" => {
                candidate_size = attr
                    .unescape_value()
                    .ok()
                    .map(|value| value.to_string())
                    .and_then(|v| v.replace(',', "").parse::<i64>().ok());
            }
            b"type" => {
                candidate_type = attr.unescape_value().ok().map(|value| value.to_string());
            }
            _ => {}
        }
    }

    let candidate_is_nzb = candidate_type
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("application/x-nzb"));
    let current_is_nzb = enclosure_type
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("application/x-nzb"));
    let should_replace = download_url.is_none() || (candidate_is_nzb && !current_is_nzb);

    if should_replace && candidate_url.is_some() {
        *download_url = candidate_url;
        *size_bytes = candidate_size;
        *enclosure_type = candidate_type;
    }
}

#[derive(Debug, Clone)]
struct NewznabCategoryOption {
    id: i64,
    name: String,
    subcategories: Vec<NewznabCategoryOption>,
}

#[derive(Debug, Default)]
struct CapsConfig {
    base_url: String,
    api_key: String,
    api_path: String,
}

pub fn execute_provider_action(input: &str) -> Result<String, Error> {
    let request: serde_json::Value = serde_json::from_str(input)?;
    let response = match action_name(&request).as_deref() {
        Some("newznabCategories") => newznab_categories(),
        _ => serde_json::json!({}),
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn newznab_categories() -> serde_json::Value {
    let categories = caps_config()
        .filter(|config| !config.base_url.is_empty() && !config.api_path.is_empty())
        .and_then(|config| fetch_categories(&config).ok());

    serde_json::json!({
        "options": category_options(categories),
    })
}

fn caps_config() -> Option<CapsConfig> {
    Some(CapsConfig {
        base_url: config::get("base_url").ok().flatten()?.trim().to_string(),
        api_key: config::get("api_key")
            .ok()
            .flatten()
            .unwrap_or_default()
            .trim()
            .to_string(),
        api_path: config::get("api_path")
            .ok()
            .flatten()
            .unwrap_or_else(|| "/api".to_string())
            .trim()
            .to_string(),
    })
}

fn fetch_categories(config: &CapsConfig) -> Result<Vec<NewznabCategoryOption>, Error> {
    let endpoint = build_endpoint(&config.base_url, &config.api_path)?;
    let mut params = vec![("t".to_string(), "caps".to_string())];
    if !config.api_key.is_empty() {
        params.push(("apikey".to_string(), config.api_key.clone()));
    }
    let url = append_query_pairs(endpoint.as_str(), &params);
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-newznab-plugin/0.1")
        .with_header("Accept", "application/rss+xml, application/xml, text/xml");
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Newznab capabilities request failed: HTTP {status}: {body}"
        )));
    }

    Ok(parse_categories(&response.body()))
}

fn append_query_pairs(base_url: &str, params: &[(String, String)]) -> String {
    let Ok(mut url) = Url::parse(base_url) else {
        return base_url.to_string();
    };
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in params {
            pairs.append_pair(key, value);
        }
    }
    url.to_string()
}

fn parse_categories(body: &[u8]) -> Vec<NewznabCategoryOption> {
    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut categories = Vec::new();
    let mut current_category: Option<NewznabCategoryOption> = None;
    let mut in_categories = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(event)) if event.name().as_ref() == b"categories" => {
                in_categories = true;
            }
            Ok(Event::End(event)) if event.name().as_ref() == b"categories" => {
                if let Some(category) = current_category.take() {
                    categories.push(category);
                }
                break;
            }
            Ok(Event::Start(event)) if in_categories && event.name().as_ref() == b"category" => {
                if let Some(category) = current_category.take() {
                    categories.push(category);
                }
                current_category = category_from_attrs(&event);
            }
            Ok(Event::Empty(event)) if in_categories && event.name().as_ref() == b"category" => {
                if let Some(category) = category_from_attrs(&event) {
                    categories.push(category);
                }
            }
            Ok(Event::Empty(event)) if in_categories && event.name().as_ref() == b"subcat" => {
                if let (Some(category), Some(subcategory)) =
                    (current_category.as_mut(), category_from_attrs(&event))
                {
                    category.subcategories.push(subcategory);
                }
            }
            Ok(Event::Start(event)) if in_categories && event.name().as_ref() == b"subcat" => {
                if let (Some(category), Some(subcategory)) =
                    (current_category.as_mut(), category_from_attrs(&event))
                {
                    category.subcategories.push(subcategory);
                }
            }
            Ok(Event::Eof) => {
                if let Some(category) = current_category.take() {
                    categories.push(category);
                }
                break;
            }
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    categories
}

fn category_from_attrs(event: &quick_xml::events::BytesStart<'_>) -> Option<NewznabCategoryOption> {
    let id = attr_value(event, b"id")?.parse::<i64>().ok()?;
    let name = attr_value(event, b"name").unwrap_or_else(|| id.to_string());
    Some(NewznabCategoryOption {
        id,
        name,
        subcategories: Vec::new(),
    })
}

fn attr_value(event: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    event
        .attributes()
        .flatten()
        .find(|attr| attr.key.as_ref() == name)
        .and_then(|attr| {
            attr.unescape_value()
                .ok()
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
}

fn category_options(categories: Option<Vec<NewznabCategoryOption>>) -> Vec<serde_json::Value> {
    let mut categories = categories.unwrap_or_else(default_newznab_categories);
    categories.retain(|category| !matches!(category.id, 1000 | 3000 | 4000 | 6000 | 7000));
    categories.sort_by_key(|category| {
        let unimportant = matches!(category.id, 0 | 2000);
        (unimportant, category.id)
    });

    let mut options = Vec::new();
    for category in categories {
        options.push(serde_json::json!({
            "value": category.id,
            "name": category.name,
            "hint": format!("({})", category.id),
        }));

        let mut subcategories = category.subcategories;
        subcategories.sort_by_key(|subcategory| subcategory.id);
        for subcategory in subcategories {
            options.push(serde_json::json!({
                "value": subcategory.id,
                "name": subcategory.name,
                "hint": format!("({})", subcategory.id),
                "parentValue": category.id,
            }));
        }
    }

    options
}

fn default_newznab_categories() -> Vec<NewznabCategoryOption> {
    vec![NewznabCategoryOption {
        id: 5000,
        name: "TV".to_string(),
        subcategories: vec![
            (5070, "Anime"),
            (5080, "Documentary"),
            (5020, "Foreign"),
            (5040, "HD"),
            (5045, "UHD"),
            (5050, "Other"),
            (5030, "SD"),
            (5060, "Sport"),
            (5010, "WEB-DL"),
        ]
        .into_iter()
        .map(|(id, name)| NewznabCategoryOption {
            id,
            name: name.to_string(),
            subcategories: Vec::new(),
        })
        .collect(),
    }]
}

fn action_name(request: &serde_json::Value) -> Option<String> {
    string_member(request, &["action", "name", "providerAction"])
}

fn string_member(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| {
            value.get(*key).and_then(|value| match value {
                serde_json::Value::String(value) => Some(value.trim().to_string()),
                serde_json::Value::Number(value) => Some(value.to_string()),
                serde_json::Value::Bool(value) => Some(value.to_string()),
                _ => None,
            })
        })
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_metadata_classification_handles_flags_and_real_values() {
        for raw in [None, Some(""), Some("  ")] {
            assert_eq!(
                classify_password_metadata(raw),
                PasswordMetadataClassification::Empty
            );
        }
        for raw in [
            Some("1"),
            Some("true"),
            Some("yes"),
            Some("passworded"),
            Some("protected"),
        ] {
            assert_eq!(
                classify_password_metadata(raw),
                PasswordMetadataClassification::ProtectedFlag
            );
        }
        for raw in [Some("0"), Some("false"), Some("no")] {
            assert_eq!(
                classify_password_metadata(raw),
                PasswordMetadataClassification::UnprotectedFlag
            );
        }
        assert_eq!(
            classify_password_metadata(Some("  actual-secret  ")),
            PasswordMetadataClassification::Real("actual-secret".to_string())
        );
    }

    #[test]
    fn password_extra_helpers_share_classification() {
        let cases = [
            (serde_json::Value::from("1"), None, Some(true)),
            (serde_json::Value::from("false"), None, Some(false)),
            (
                serde_json::Value::from("actual-secret"),
                Some("actual-secret".to_string()),
                Some(true),
            ),
            (serde_json::Value::from(true), None, Some(true)),
            (serde_json::Value::from(false), None, Some(false)),
        ];

        for (value, expected_password, expected_protected) in cases {
            let extra = HashMap::from([("password".to_string(), value)]);
            assert_eq!(password_hint_from_extra(&extra), expected_password);
            assert_eq!(protected_from_extra(&extra), expected_protected);
        }
    }

    fn assert_parses_as_http_uri(url: &str) {
        let _: ::http::Uri = url.parse().expect("URL should parse as http::Uri");
    }

    fn query_value(url: &str, key: &str) -> Option<String> {
        Url::parse(url)
            .ok()?
            .query_pairs()
            .find_map(|(candidate, value)| (candidate == key).then(|| value.into_owned()))
    }

    fn extract_torrent_test_metadata(
        pairs: &[(String, String)],
    ) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
        let mut extra = HashMap::new();
        let mut tags = Vec::new();

        for (name, value) in pairs {
            let normalized = name
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            let trimmed = value.trim();

            match normalized.as_str() {
                "seeders" | "leechers" | "peers" | "minimumseedtime" => {
                    if let Ok(value) = trimmed.parse::<i64>() {
                        extra.insert(normalized, serde_json::Value::from(value));
                    }
                }
                "downloadvolumefactor" | "uploadvolumefactor" | "minimumratio" => {
                    if let Ok(value) = trimmed.parse::<f64>() {
                        extra.insert(normalized, serde_json::Value::from(value));
                    }
                }
                "infohash" => {
                    extra.insert(
                        "info_hash".to_string(),
                        serde_json::Value::from(trimmed.to_ascii_lowercase()),
                    );
                }
                "magneturl" => {
                    extra.insert("magnet_uri".to_string(), serde_json::Value::from(trimmed));
                }
                "tag" => tags.push(trimmed.to_string()),
                _ => {}
            }
        }

        if !tags.is_empty() {
            extra.insert("tags".to_string(), serde_json::json!(tags));
        }

        (Vec::new(), None, extra)
    }

    // ── determine_search_type ────────────────────────────────────────────

    #[test]
    fn search_type_movie_category() {
        let cats = vec!["2000".into()];
        assert_eq!(
            determine_search_type(&cats, None, None, None, None, None, None),
            "movie"
        );
    }

    #[test]
    fn search_type_tv_category() {
        let cats = vec!["5000".into()];
        assert_eq!(
            determine_search_type(&cats, None, None, None, None, None, None),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_movie_hint() {
        assert_eq!(
            determine_search_type(&[], None, Some("movie"), None, None, None, None),
            "movie"
        );
    }

    #[test]
    fn search_type_tv_hint() {
        assert_eq!(
            determine_search_type(&[], None, Some("series"), None, None, None, None),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_anime_hint() {
        assert_eq!(
            determine_search_type(&[], None, Some("anime"), None, None, None, None),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_prefers_explicit_facet() {
        assert_eq!(
            determine_search_type(&[], Some("series"), Some("movie"), None, None, None, None),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_imdb_id_fallback() {
        assert_eq!(
            determine_search_type(&[], None, None, Some("1234567"), None, None, None),
            "movie"
        );
    }

    #[test]
    fn search_type_tvdb_id_fallback() {
        assert_eq!(
            determine_search_type(&[], None, None, None, Some("12345"), None, None),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_tvmaze_and_tvrage_id_fallbacks() {
        assert_eq!(
            determine_search_type(&[], None, None, None, None, Some("70399"), None),
            "tvsearch"
        );
        assert_eq!(
            determine_search_type(&[], None, None, None, None, None, Some("39852")),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_generic_fallback() {
        assert_eq!(
            determine_search_type(&[], None, None, None, None, None, None),
            "search"
        );
    }

    #[test]
    fn typed_movie_context_overrides_legacy_anime_category_hint() {
        let req = SearchRequest {
            query: String::new(),
            ids: std::collections::HashMap::from([(
                "imdb_id".to_string(),
                "tt11032374".to_string(),
            )]),
            facet: Some("movie".to_string()),
            category: Some("anime".to_string()),
            categories: vec!["5070".to_string(), "2000".to_string()],
            context: Some(scryer_plugin_sdk::PluginSearchContext {
                subject_kind: PluginSearchSubjectKind::Movie,
                ..scryer_plugin_sdk::PluginSearchContext::default()
            }),
            ..SearchRequest::default()
        };

        assert_eq!(
            determine_nab_search_shape(
                &req,
                NabSearchShapeHints {
                    categories: &req.categories,
                    facet: req.facet.as_deref(),
                    category: req.category.as_deref(),
                    imdb_id: Some("tt11032374"),
                    tvdb_id: None,
                    tvrage_id: None,
                    tvmaze_id: None,
                },
            ),
            NabSearchShape::Movie
        );
    }

    #[test]
    fn typed_movie_context_with_full_series_movie_ids_stays_movie_shape() {
        let req = SearchRequest {
            query: String::new(),
            ids: std::collections::HashMap::from([
                ("imdb_id".to_string(), "tt11032374".to_string()),
                ("tmdb_id".to_string(), "635302".to_string()),
                ("tvdb_id".to_string(), "131963".to_string()),
                ("anidb_id".to_string(), "15646".to_string()),
                ("mal_id".to_string(), "40456".to_string()),
            ]),
            facet: Some("movie".to_string()),
            category: Some("movie".to_string()),
            categories: vec!["6050".to_string(), "2000".to_string(), "5070".to_string()],
            context: Some(scryer_plugin_sdk::PluginSearchContext {
                subject_kind: PluginSearchSubjectKind::Movie,
                ..scryer_plugin_sdk::PluginSearchContext::default()
            }),
            ..SearchRequest::default()
        };

        assert_eq!(
            determine_nab_search_shape(
                &req,
                NabSearchShapeHints {
                    categories: &req.categories,
                    facet: req.facet.as_deref(),
                    category: req.category.as_deref(),
                    imdb_id: Some("tt11032374"),
                    tvdb_id: Some("131963"),
                    tvrage_id: None,
                    tvmaze_id: None,
                },
            ),
            NabSearchShape::Movie
        );
    }

    #[test]
    fn typed_movie_context_with_only_anime_ids_does_not_become_anime_exact() {
        let req = SearchRequest {
            query: String::new(),
            ids: std::collections::HashMap::from([
                ("anidb_id".to_string(), "15646".to_string()),
                ("mal_id".to_string(), "40456".to_string()),
            ]),
            facet: Some("movie".to_string()),
            category: Some("movie".to_string()),
            categories: vec!["5070".to_string(), "2000".to_string()],
            context: Some(scryer_plugin_sdk::PluginSearchContext {
                subject_kind: PluginSearchSubjectKind::Movie,
                ..scryer_plugin_sdk::PluginSearchContext::default()
            }),
            ..SearchRequest::default()
        };

        assert_eq!(
            determine_nab_search_shape(
                &req,
                NabSearchShapeHints {
                    categories: &req.categories,
                    facet: req.facet.as_deref(),
                    category: req.category.as_deref(),
                    imdb_id: None,
                    tvdb_id: None,
                    tvrage_id: None,
                    tvmaze_id: None,
                },
            ),
            NabSearchShape::Movie
        );
    }

    #[test]
    fn legacy_anime_category_hint_still_uses_anime_exact_shape() {
        let req = SearchRequest {
            query: "Demon Slayer".to_string(),
            category: Some("anime".to_string()),
            categories: vec!["5070".to_string()],
            ..SearchRequest::default()
        };

        assert_eq!(
            determine_nab_search_shape(
                &req,
                NabSearchShapeHints {
                    categories: &req.categories,
                    facet: req.facet.as_deref(),
                    category: req.category.as_deref(),
                    imdb_id: None,
                    tvdb_id: Some("131963"),
                    tvrage_id: None,
                    tvmaze_id: None,
                },
            ),
            NabSearchShape::AnimeExact
        );
    }

    #[test]
    fn typed_anime_episode_context_uses_anime_exact_shape() {
        let req = SearchRequest {
            query: "Demon Slayer".to_string(),
            facet: Some("anime".to_string()),
            category: Some("anime".to_string()),
            categories: vec!["5070".to_string()],
            episode: Some(1),
            context: Some(scryer_plugin_sdk::PluginSearchContext {
                subject_kind: PluginSearchSubjectKind::AnimeEpisode,
                ..scryer_plugin_sdk::PluginSearchContext::default()
            }),
            ..SearchRequest::default()
        };

        assert_eq!(
            determine_nab_search_shape(
                &req,
                NabSearchShapeHints {
                    categories: &req.categories,
                    facet: req.facet.as_deref(),
                    category: req.category.as_deref(),
                    imdb_id: None,
                    tvdb_id: Some("131963"),
                    tvrage_id: None,
                    tvmaze_id: None,
                },
            ),
            NabSearchShape::AnimeExact
        );
    }

    // ── build_category_param ─────────────────────────────────────────────

    #[test]
    fn category_param_numeric_only() {
        let cats: Vec<String> = vec!["2000".into(), "movie".into(), "5040".into()];
        assert_eq!(build_category_param(&cats), Some("2000,5040".into()));
    }

    #[test]
    fn category_param_empty() {
        let cats: Vec<String> = vec![];
        assert_eq!(build_category_param(&cats), None);
    }

    #[test]
    fn category_param_all_non_numeric() {
        let cats: Vec<String> = vec!["movie".into(), "tv".into()];
        assert_eq!(build_category_param(&cats), None);
    }

    #[test]
    fn build_search_url_movie_prefers_query_and_imdbid() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "movie",
            Some("12 years a slave"),
            "test-api-key",
            Some("tt2024544"),
            None,
            None,
            None,
            None,
            None,
            200,
            None,
            None,
            "",
        )
        .unwrap();

        assert!(url.contains("t=movie"));
        assert_eq!(query_value(&url, "q").as_deref(), Some("12 years a slave"));
        assert!(url.contains("imdbid=002024544"));
        assert!(url.contains("limit=200"));
        assert!(url.contains("extended=1"));
        assert!(query_value(&url, "o").is_none());
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_search_url_movie_id_search_uses_imdbid_and_category_filter_without_query() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "movie",
            None,
            "test-api-key",
            Some("tt11032374"),
            None,
            None,
            None,
            None,
            Some("6050,2000,5070"),
            200,
            None,
            None,
            "",
        )
        .unwrap();

        assert_eq!(query_value(&url, "t").as_deref(), Some("movie"));
        assert_eq!(query_value(&url, "imdbid").as_deref(), Some("011032374"));
        assert_eq!(query_value(&url, "cat").as_deref(), Some("6050,2000,5070"));
        assert!(query_value(&url, "q").is_none());
        assert_eq!(
            query_value(&redact_url_for_log(&url), "apikey").as_deref(),
            Some("REDACTED")
        );
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_search_url_tv_prefers_query_and_tvdbid() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "tvsearch",
            Some("demon slayer"),
            "test-api-key",
            None,
            None,
            Some("123456"),
            None,
            None,
            None,
            200,
            None,
            None,
            "",
        )
        .unwrap();

        assert!(url.contains("t=tvsearch"));
        assert_eq!(query_value(&url, "q").as_deref(), Some("demon slayer"));
        assert!(url.contains("tvdbid=123456"));
        assert!(url.contains("limit=200"));
        assert!(url.contains("extended=1"));
        assert!(query_value(&url, "o").is_none());
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_query_variants_prefers_romanized_anime_alias_before_canonical() {
        let variants = build_query_variants("Silver Horizon S01E01");
        assert_eq!(variants, vec!["Silver Horizon"]);
    }

    #[test]
    fn build_search_url_tvdb_absolute_omits_query_and_season() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "tvsearch",
            None,
            "test-api-key",
            None,
            None,
            Some("74796"),
            None,
            None,
            None,
            200,
            None,
            Some(403),
            "",
        )
        .unwrap();

        assert!(url.contains("tvdbid=74796"));
        assert!(url.contains("ep=403"));
        assert!(!url.contains("q="));
        assert!(!url.contains("season="));
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_search_url_tvsearch_includes_tvdbid_and_episode_hints() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "tvsearch",
            Some("YATAGARASU: The Raven Does Not Choose Its Master"),
            "test-api-key",
            None,
            None,
            Some("422695"),
            None,
            None,
            None,
            200,
            Some(1),
            Some(18),
            "",
        )
        .unwrap();

        assert_eq!(
            query_value(&url, "q").as_deref(),
            Some("YATAGARASU: The Raven Does Not Choose Its Master")
        );
        assert_eq!(query_value(&url, "tvdbid").as_deref(), Some("422695"));
        assert_eq!(query_value(&url, "season").as_deref(), Some("1"));
        assert_eq!(query_value(&url, "ep").as_deref(), Some("18"));
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_search_url_anime_freetext_uses_title_only_with_episode_context() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "tvsearch",
            Some("Sora no Vale"),
            "test-api-key",
            None,
            None,
            None,
            None,
            None,
            None,
            200,
            Some(1),
            Some(1),
            "",
        )
        .unwrap();

        assert_eq!(query_value(&url, "q").as_deref(), Some("Sora no Vale"));
        assert!(url.contains("season=1"));
        assert!(url.contains("ep=1"));
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_search_url_trims_and_reencodes_whitespace_padded_values() {
        let endpoint = build_endpoint("  https://api.nzbgeek.info/  ", " /api/ ").unwrap();
        let url = build_search_url(
            &endpoint,
            "movie",
            Some("The Batman"),
            "  test-api-key \n",
            Some("tt1877830"),
            None,
            Some(" 12345 \n"),
            Some(" 70399 \n"),
            Some(" 39852 \n"),
            Some(" 2000 , 2040 "),
            100,
            None,
            None,
            " \n&dl=1&attrs=poster image \n",
        )
        .unwrap();

        assert_eq!(query_value(&url, "apikey").as_deref(), Some("test-api-key"));
        assert_eq!(query_value(&url, "tvdbid").as_deref(), Some("12345"));
        assert_eq!(query_value(&url, "rid").as_deref(), Some("70399"));
        assert_eq!(query_value(&url, "tvmazeid").as_deref(), Some("39852"));
        assert_eq!(query_value(&url, "cat").as_deref(), Some("2000 , 2040"));
        assert_eq!(query_value(&url, "attrs").as_deref(), Some("poster image"));
        assert_eq!(query_value(&url, "dl").as_deref(), Some("1"));
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_search_url_accepts_additional_params_with_or_without_prefix_markers() {
        for additional_params in [
            "dl=1&attrs=poster",
            "&dl=1&attrs=poster",
            "?dl=1&attrs=poster",
        ] {
            let url = build_search_url(
                "https://api.nzbgeek.info/api",
                "movie",
                None,
                "test-api-key",
                None,
                None,
                None,
                None,
                None,
                None,
                25,
                None,
                None,
                additional_params,
            )
            .unwrap();

            assert_eq!(query_value(&url, "dl").as_deref(), Some("1"));
            assert_eq!(query_value(&url, "attrs").as_deref(), Some("poster"));
            assert_parses_as_http_uri(&url);
        }
    }

    #[test]
    fn build_search_url_does_not_double_encode_additional_params() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "movie",
            None,
            "test-api-key",
            None,
            None,
            None,
            None,
            None,
            None,
            25,
            None,
            None,
            "attrs=poster%20image&label=dual+audio",
        )
        .unwrap();

        assert_eq!(query_value(&url, "attrs").as_deref(), Some("poster image"));
        assert_eq!(query_value(&url, "label").as_deref(), Some("dual audio"));
        assert!(!url.contains("%2520"));
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn build_search_url_includes_tmdbid_when_present() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "movie",
            Some("Dune Part Two"),
            "test-api-key",
            None,
            Some("693134"),
            None,
            None,
            None,
            None,
            50,
            None,
            None,
            "",
        )
        .unwrap();

        assert_eq!(query_value(&url, "tmdbid").as_deref(), Some("693134"));
        assert_eq!(query_value(&url, "q").as_deref(), Some("Dune Part Two"));
        assert_parses_as_http_uri(&url);
    }

    #[test]
    fn redact_url_for_log_redacts_apikey() {
        let redacted =
            redact_url_for_log("https://example.test/api?t=movie&apikey=secret&o=json&token=abc");
        assert!(redacted.contains("apikey=REDACTED"));
        assert!(redacted.contains("token=REDACTED"));
        assert!(redacted.contains("t=movie"));
    }

    #[test]
    fn success_status_rejects_redirects() {
        assert!(is_success_status(200));
        assert!(is_success_status(204));
        assert!(!is_success_status(302));
        assert!(!is_success_status(500));
    }

    #[test]
    fn normalize_imdbid_param_defaults_to_legacy_newznab_format() {
        assert_eq!(
            normalize_imdbid_param_with_mode("tt2024544", false),
            "002024544"
        );
        assert_eq!(
            normalize_imdbid_param_with_mode("TT1234567", false),
            "001234567"
        );
        assert_eq!(
            normalize_imdbid_param_with_mode("1234567", false),
            "1234567"
        );
    }

    #[test]
    fn normalize_imdbid_param_supports_canonical_tt_format_for_proxy_configs() {
        assert_eq!(
            normalize_imdbid_param_with_mode("tt2024544", true),
            "tt2024544"
        );
        assert_eq!(
            normalize_imdbid_param_with_mode("TT1234567", true),
            "tt1234567"
        );
        assert_eq!(
            normalize_imdbid_param_with_mode("1234567", true),
            "tt1234567"
        );
    }

    #[test]
    fn category_param_whitespace_trimmed() {
        let cats: Vec<String> = vec![" 2000 ".into(), "5040".into()];
        assert_eq!(build_category_param(&cats), Some("2000,5040".into()));
    }

    // ── build_endpoint ───────────────────────────────────────────────────

    #[test]
    fn endpoint_normal() {
        assert_eq!(
            build_endpoint("https://api.nzbgeek.info", "/api").unwrap(),
            "https://api.nzbgeek.info/api"
        );
    }

    #[test]
    fn endpoint_trailing_slash() {
        assert_eq!(
            build_endpoint("https://example.com/", "/api/").unwrap(),
            "https://example.com/api"
        );
    }

    #[test]
    fn endpoint_empty_path() {
        assert_eq!(
            build_endpoint("https://example.com", "").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn endpoint_custom_path() {
        assert_eq!(
            build_endpoint("https://foo.bar", "/api/v1/api").unwrap(),
            "https://foo.bar/api/v1/api"
        );
    }

    #[test]
    fn endpoint_trims_whitespace_and_discards_query_string() {
        assert_eq!(
            build_endpoint("  https://example.com/base/?stale=true  ", " /api/v1/ ").unwrap(),
            "https://example.com/base/api/v1"
        );
    }

    // ── is_empty_response ────────────────────────────────────────────────

    #[test]
    fn empty_json_no_title() {
        assert!(is_empty_response(r#"{"channel":{}}"#));
    }

    #[test]
    fn non_empty_json() {
        assert!(!is_empty_response(
            r#"{"channel":{"item":{"title":"foo"}}}"#
        ));
    }

    #[test]
    fn empty_xml_no_item() {
        assert!(is_empty_response("<rss><channel></channel></rss>"));
    }

    #[test]
    fn non_empty_xml() {
        assert!(!is_empty_response(
            "<rss><channel><item><title>foo</title></item></channel></rss>"
        ));
    }

    #[test]
    fn random_text_is_empty() {
        assert!(is_empty_response("hello"));
    }

    // ── url_encode ───────────────────────────────────────────────────────

    #[test]
    fn encode_plain_ascii() {
        assert_eq!(url_encode("hello"), "hello");
    }

    #[test]
    fn encode_spaces() {
        assert_eq!(url_encode("hello world"), "hello%20world");
    }

    #[test]
    fn encode_special_chars() {
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn encode_unreserved() {
        assert_eq!(url_encode("-_.~"), "-_.~");
    }

    // ── classify_and_format_error ────────────────────────────────────────

    #[test]
    fn error_api_key_range() {
        let err = classify_and_format_error("100", "Invalid API key");
        let msg = format!("{err}");
        assert!(msg.starts_with("Newznab API key error"), "got: {msg}");
    }

    #[test]
    fn error_rate_limit() {
        let err = classify_and_format_error("500", "Request limit reached");
        let msg = format!("{err}");
        assert!(msg.starts_with("Newznab rate limit"), "got: {msg}");
    }

    #[test]
    fn error_generic() {
        let err = classify_and_format_error("300", "something went wrong");
        let msg = format!("{err}");
        assert!(msg.starts_with("Newznab error"), "got: {msg}");
    }

    // ── apply_standard_attrs ─────────────────────────────────────────────

    fn make_result() -> SearchResult {
        SearchResult {
            title: "test".into(),
            link: None,
            download_url: None,
            size_bytes: None,
            published_at: None,
            grabs: None,
            languages: vec![],
            thumbs_up: None,
            thumbs_down: None,
            subtitles: vec![],
            password_hint: None,
            protected: None,
            provider_extra: HashMap::new(),
            guid: None,
            info_url: None,
            ..SearchResult::default()
        }
    }

    #[test]
    fn attrs_usenet_date() {
        let pairs = vec![("usenetdate".into(), "2024-01-15".into())];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        assert_eq!(usenet_date, Some("2024-01-15".into()));
    }

    #[test]
    fn attrs_tvdbid() {
        let pairs = vec![("tvdbid".into(), "12345".into())];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        assert_eq!(
            result.provider_extra.get("response_tvdbid"),
            Some(&serde_json::Value::from("12345"))
        );
    }

    #[test]
    fn attrs_imdbid() {
        let pairs = vec![("imdb".into(), "1234567".into())];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        assert_eq!(
            result.provider_extra.get("response_imdbid"),
            Some(&serde_json::Value::from("1234567"))
        );
    }

    #[test]
    fn attrs_prematch() {
        let pairs = vec![("prematch".into(), "1".into())];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        let flags = result.provider_extra.get("indexer_flags").unwrap();
        assert!(
            flags
                .as_array()
                .unwrap()
                .contains(&serde_json::Value::from("scene"))
        );
    }

    #[test]
    fn attrs_nuked() {
        let pairs = vec![("nuked".into(), "1".into())];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        let flags = result.provider_extra.get("indexer_flags").unwrap();
        assert!(
            flags
                .as_array()
                .unwrap()
                .contains(&serde_json::Value::from("nuked"))
        );
    }

    #[test]
    fn attrs_ignores_zero_values() {
        let pairs = vec![
            ("tvdbid".into(), "0".into()),
            ("imdb".into(), "0".into()),
            ("prematch".into(), "0".into()),
            ("nuked".into(), "0".into()),
        ];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        assert!(
            result.provider_extra.is_empty(),
            "got: {:?}",
            result.provider_extra
        );
    }

    // ── parse_error_json ─────────────────────────────────────────────────

    #[test]
    fn json_error_present() {
        let body = r#"{"error":{"@attributes":{"code":"100","description":"bad key"}}}"#;
        let result = parse_error_json(body);
        assert_eq!(result, Some(("100".into(), "bad key".into())));
    }

    #[test]
    fn json_error_absent() {
        let body = r#"{"channel":{}}"#;
        assert_eq!(parse_error_json(body), None);
    }

    #[test]
    fn json_account_invalid_key_status_is_error() {
        let body = r#"{"channel":{"account":{"@attributes":{"status":"Invalid API Key"}}}}"#;
        let result = parse_error_json(body);
        assert_eq!(result, Some(("100".into(), "Invalid API Key".into())));
    }

    #[test]
    fn json_account_ok_status_is_not_error() {
        let body = r#"{"channel":{"account":{"@attributes":{"status":"OK"}}}}"#;
        assert_eq!(parse_error_json(body), None);
    }

    #[test]
    fn json_malformed() {
        assert_eq!(parse_error_json("not json"), None);
    }

    // ── parse_error_xml ──────────────────────────────────────────────────

    #[test]
    fn xml_error_present() {
        let body = r#"<?xml version="1.0"?><error code="100" description="bad key"/>"#;
        let result = parse_error_xml(body);
        assert_eq!(result, Some(("100".into(), "bad key".into())));
    }

    #[test]
    fn xml_error_absent() {
        let body = "<rss><channel></channel></rss>";
        assert_eq!(parse_error_xml(body), None);
    }

    #[test]
    fn xml_account_invalid_key_status_is_error() {
        let body = r#"<rss><channel><account status="Invalid API Key"/></channel></rss>"#;
        let result = parse_error_xml(body);
        assert_eq!(result, Some(("100".into(), "Invalid API Key".into())));
    }

    #[test]
    fn xml_error_partial() {
        let body = r#"<?xml version="1.0"?><error code="100"/>"#;
        let result = parse_error_xml(body);
        assert_eq!(result, Some(("100".into(), "unknown".into())));
    }

    // ── parse_newznab_json ───────────────────────────────────────────────

    #[test]
    fn json_empty_channel() {
        let body = r#"{"channel":{}}"#;
        let (results, _) = parse_newznab_json(body, 100, extract_base_metadata);
        assert!(results.is_empty());
    }

    #[test]
    fn json_single_item() {
        let body = r#"{
            "channel": {
                "item": {
                    "title": "Test.Release.720p",
                    "guid": "abc123",
                    "link": "https://example.com/details/abc123",
                    "comments": "https://example.com/details/abc123#comments",
                    "pubDate": "Mon, 01 Jan 2024 12:00:00 +0000",
                    "enclosure": {
                        "@attributes": {
                            "url": "https://example.com/download/abc123",
                            "length": "1073741824",
                            "type": "application/x-nzb"
                        }
                    },
                    "attr": [
                        {"@attributes": {"name": "grabs", "value": "42"}},
                        {"@attributes": {"name": "language", "value": "English"}}
                    ]
                }
            }
        }"#;
        let (results, _) = parse_newznab_json(body, 100, extract_base_metadata);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.title, "Test.Release.720p");
        assert_eq!(r.guid.as_deref(), Some("abc123"));
        assert_eq!(
            r.download_url.as_deref(),
            Some("https://example.com/download/abc123")
        );
        assert_eq!(r.size_bytes, Some(1_073_741_824));
        assert_eq!(r.grabs, Some(42));
        assert_eq!(r.languages, vec!["English"]);
        assert_eq!(
            r.info_url.as_deref(),
            Some("https://example.com/details/abc123")
        );
    }

    #[test]
    fn json_multiple_items_respects_limit() {
        let body = r#"{
            "channel": {
                "item": [
                    {"title": "Item 1", "guid": "a"},
                    {"title": "Item 2", "guid": "b"},
                    {"title": "Item 3", "guid": "c"}
                ]
            }
        }"#;
        let (results, _) = parse_newznab_json(body, 2, extract_base_metadata);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn json_item_without_title_skipped() {
        let body = r#"{"channel":{"item":{"guid":"abc"}}}"#;
        let (results, _) = parse_newznab_json(body, 100, extract_base_metadata);
        assert!(results.is_empty());
    }

    #[test]
    fn json_api_limits_extracted() {
        let body = r#"{
            "channel": {},
            "limits": {
                "@attributes": {
                    "api_current": "5",
                    "api_max": "100",
                    "grab_current": "10",
                    "grab_max": "500"
                }
            }
        }"#;
        let (_, limits) = parse_newznab_json(body, 100, extract_base_metadata);
        assert_eq!(limits.api_current, Some(5));
        assert_eq!(limits.api_max, Some(100));
        assert_eq!(limits.grab_current, Some(10));
        assert_eq!(limits.grab_max, Some(500));
    }

    // ── parse_newznab_xml ────────────────────────────────────────────────

    #[test]
    fn xml_empty_rss() {
        let body = r#"<?xml version="1.0"?><rss><channel></channel></rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert!(results.is_empty());
    }

    #[test]
    fn xml_single_item() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>Test.Release.1080p</title>
    <guid>def456</guid>
    <link>https://example.com/details/def456</link>
    <comments>https://example.com/details/def456#comments</comments>
    <pubDate>Tue, 02 Jan 2024 14:00:00 +0000</pubDate>
    <enclosure url="https://example.com/dl/def456" length="2147483648" type="application/x-nzb"/>
    <newznab:attr name="grabs" value="99"/>
    <newznab:attr name="language" value="English - French"/>
  </item>
</channel>
</rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.title, "Test.Release.1080p");
        assert_eq!(r.guid.as_deref(), Some("def456"));
        assert_eq!(
            r.download_url.as_deref(),
            Some("https://example.com/dl/def456")
        );
        assert_eq!(r.size_bytes, Some(2_147_483_648));
        assert_eq!(r.grabs, Some(99));
        assert_eq!(r.languages, vec!["English", "French"]);
        assert_eq!(
            r.info_url.as_deref(),
            Some("https://example.com/details/def456")
        );
    }

    #[test]
    fn xml_extracts_password_hint() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>Protected.Release.1080p</title>
    <newznab:attr name="password" value=" archive-password "/>
  </item>
</channel>
</rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].password_hint.as_deref(),
            Some("archive-password")
        );
        assert_eq!(
            results[0]
                .provider_extra
                .get("password")
                .and_then(|value| value.as_str()),
            Some("archive-password")
        );
        assert_eq!(
            results[0]
                .provider_extra
                .get("password_protected")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn xml_treats_password_flags_as_protection_only() {
        for marker in ["1", "true", "yes", "passworded", "protected"] {
            let body = format!(
                r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>Protected.Release.1080p</title>
    <newznab:attr name="password" value="{marker}"/>
  </item>
</channel>
</rss>"#
            );
            let (results, _) = parse_newznab_xml(&body, 100, extract_base_metadata);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].password_hint, None);
            assert!(!results[0].provider_extra.contains_key("password"));
            assert_eq!(
                results[0]
                    .provider_extra
                    .get("password_protected")
                    .and_then(|value| value.as_bool()),
                Some(true),
                "marker {marker:?} should be protection metadata only"
            );
        }
    }

    #[test]
    fn xml_ignores_zero_password_hint() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>Unprotected.Release.1080p</title>
    <newznab:attr name="password" value="0"/>
  </item>
</channel>
</rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].password_hint, None);
        assert!(!results[0].provider_extra.contains_key("password"));
        assert_eq!(
            results[0]
                .provider_extra
                .get("password_protected")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn xml_maps_torrent_extra_to_typed_fields() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>Torrent.Release.1080p</title>
    <enclosure url="https://example.com/dl/release.torrent" length="1234" type="application/x-bittorrent"/>
    <newznab:attr name="seeders" value="42"/>
    <newznab:attr name="leechers" value="9"/>
    <newznab:attr name="downloadvolumefactor" value="0.5"/>
    <newznab:attr name="uploadvolumefactor" value="2"/>
    <newznab:attr name="minimumratio" value="1.5"/>
    <newznab:attr name="minimumseedtime" value="60"/>
    <newznab:attr name="infohash" value="ABCDEF1234567890ABCDEF1234567890ABCDEF12"/>
    <newznab:attr name="magneturl" value="magnet:?xt=urn:btih:abcdef"/>
    <newznab:attr name="tag" value="internal"/>
  </item>
</channel>
</rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_torrent_test_metadata);
        assert_eq!(results.len(), 1);
        let r = &results[0];

        assert_eq!(r.seeders, Some(42));
        assert_eq!(r.leechers, Some(9));
        assert_eq!(r.peers, Some(51));
        assert_eq!(r.download_volume_factor, Some(0.5));
        assert_eq!(r.upload_volume_factor, Some(2.0));
        assert_eq!(r.minimum_seed_ratio, Some(1.5));
        assert_eq!(r.minimum_seed_time_minutes, Some(60));
        assert_eq!(
            r.info_hash_v1.as_deref(),
            Some("abcdef1234567890abcdef1234567890abcdef12")
        );
        assert_eq!(r.magnet_url.as_deref(), Some("magnet:?xt=urn:btih:abcdef"));
        assert!(r.indexer_flags.iter().any(|flag| flag == "halfleech"));
        assert!(r.indexer_flags.iter().any(|flag| flag == "doubleupload"));
        assert!(r.indexer_flags.iter().any(|flag| flag == "internal"));
    }

    #[test]
    fn xml_enclosure_url_unescapes_query_delimiters() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>Escaped.Link.Release</title>
    <enclosure url="http://localhost:9696/1/download?apikey=test&amp;link=abc123&amp;file=Escaped.Link.Release" length="1024" type="application/x-nzb"/>
  </item>
</channel>
</rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].download_url.as_deref(),
            Some(
                "http://localhost:9696/1/download?apikey=test&link=abc123&file=Escaped.Link.Release"
            )
        );
    }

    #[test]
    fn xml_prefers_nzb_enclosure_over_torrent_enclosure() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>AnimeTosho.Style.Release</title>
    <enclosure url="https://storage.example/torrent/release.torrent" length="0" type="application/x-bittorrent"/>
    <enclosure url="https://storage.example/nzbs/release.nzb" length="0" type="application/x-nzb"/>
    <newznab:attr name="size" value="123456789"/>
  </item>
</channel>
</rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].download_url.as_deref(),
            Some("https://storage.example/nzbs/release.nzb")
        );
        assert_eq!(results[0].size_bytes, Some(123456789));
        assert!(!results[0].provider_extra.contains_key("enclosure_type"));
    }

    #[test]
    fn xml_multiple_items_respects_limit() {
        let body = r#"<?xml version="1.0"?>
<rss><channel>
  <item><title>A</title></item>
  <item><title>B</title></item>
  <item><title>C</title></item>
</channel></rss>"#;
        let (results, _) = parse_newznab_xml(body, 1, extract_base_metadata);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "A");
    }

    #[test]
    fn xml_size_fallback_from_attr() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>Fallback Size</title>
    <enclosure url="https://example.com/dl/x" length="0" type="application/x-nzb"/>
    <newznab:attr name="size" value="12345"/>
  </item>
</channel>
</rss>"#;
        let (results, _) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].size_bytes, Some(12345));
    }

    #[test]
    fn xml_limits_parsed() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <newznab:limits api_current="5" api_max="100" grab_current="10" grab_max="500"/>
</channel>
</rss>"#;
        let (results, limits) = parse_newznab_xml(body, 100, extract_base_metadata);
        assert!(results.is_empty());
        assert_eq!(limits.api_current, Some(5));
        assert_eq!(limits.api_max, Some(100));
        assert_eq!(limits.grab_current, Some(10));
        assert_eq!(limits.grab_max, Some(500));
    }

    // ── extract_base_metadata ────────────────────────────────────────────

    #[test]
    fn base_extracts_language_and_grabs() {
        let pairs = vec![
            ("language".into(), "English - French".into()),
            ("grabs".into(), "1,234".into()),
        ];
        let (languages, grabs, extra) = extract_base_metadata(&pairs);
        assert_eq!(languages, vec!["English", "French"]);
        assert_eq!(grabs, Some(1234));
        assert!(extra.is_empty());
    }

    #[test]
    fn base_ignores_unknown_attrs() {
        let pairs = vec![("foo".into(), "bar".into())];
        let (languages, grabs, extra) = extract_base_metadata(&pairs);
        assert!(languages.is_empty());
        assert_eq!(grabs, None);
        assert!(extra.is_empty());
    }

    #[test]
    fn base_extracts_password_hint() {
        let pairs = vec![("password".into(), " correct horse battery staple ".into())];
        let (_, _, extra) = extract_base_metadata(&pairs);
        assert_eq!(
            extra.get("password"),
            Some(&serde_json::Value::from("correct horse battery staple"))
        );
        assert_eq!(
            extra.get("password_protected"),
            Some(&serde_json::Value::from(true))
        );
    }

    #[test]
    fn base_ignores_empty_and_zero_password_hints() {
        for value in ["", "  "] {
            let pairs = vec![("password".into(), value.into())];
            let (_, _, extra) = extract_base_metadata(&pairs);
            assert!(
                !extra.contains_key("password"),
                "password attr {value:?} should not become a password hint"
            );
            assert!(!extra.contains_key("password_protected"));
        }

        for value in ["0", " 0 ", "false", "no"] {
            let pairs = vec![("password".into(), value.into())];
            let (_, _, extra) = extract_base_metadata(&pairs);
            assert!(
                !extra.contains_key("password"),
                "password attr {value:?} should not become a password hint"
            );
            assert_eq!(
                extra.get("password_protected"),
                Some(&serde_json::Value::from(false))
            );
        }
    }

    #[test]
    fn base_treats_password_flags_as_protection_only() {
        for value in ["1", "true", "yes", "passworded", "protected"] {
            let pairs = vec![("password".into(), value.into())];
            let (_, _, extra) = extract_base_metadata(&pairs);
            assert!(
                !extra.contains_key("password"),
                "password attr {value:?} should not become a password hint"
            );
            assert_eq!(
                extra.get("password_protected"),
                Some(&serde_json::Value::from(true))
            );
        }
    }

    // ── standard_config_fields ───────────────────────────────────────────

    #[test]
    fn config_fields_has_api_path_and_additional_params() {
        let fields = standard_config_fields(None);
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0].key, "base_url");
        assert!(fields[0].required);
        assert_eq!(fields[1].key, "api_key");
        assert!(fields[1].required);
        assert_eq!(fields[2].key, "api_path");
        assert_eq!(fields[3].key, "additional_params");
    }
}
