//! Shared Newznab protocol engine for indexer plugins.
//!
//! This crate provides the core Newznab/Torznab API client logic used by
//! both the generic `newznab` and the NZBGeek-specific `nzbgeek` plugins.
//! Each plugin is a thin wrapper that calls [`execute_full_search`] with
//! a provider-specific [`MetadataExtractor`] callback.

use std::collections::HashMap;

use extism_pdk::*;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Plugin contract types (must match scryer-plugins/src/types.rs)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PluginDescriptor {
    pub name: String,
    pub version: String,
    pub sdk_version: String,
    pub plugin_type: String,
    pub provider_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_aliases: Vec<String>,
    pub capabilities: Capabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scoring_policies: Vec<ScoringPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_fields: Vec<ConfigFieldDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_seconds: Option<i64>,
}

#[derive(Serialize)]
pub struct Capabilities {
    /// Facet-scoped ID support using well-known names: e.g. {"movie": ["imdb_id"], "series": ["tvdb_id"]}
    pub supported_ids: HashMap<String, Vec<String>>,
    /// Whether this indexer deduplicates title aliases internally.
    #[serde(default)]
    pub deduplicates_aliases: bool,
    /// Query param name for season filtering (e.g. "season").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_param: Option<String>,
    /// Query param name for episode filtering (e.g. "ep").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_param: Option<String>,
    /// Query param name for freetext search (e.g. "q"). None = no freetext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_param: Option<String>,
    // Legacy fields for backward compat with host-side deserialization.
    pub search: bool,
    pub imdb_search: bool,
    pub tvdb_search: bool,
}

#[derive(Serialize)]
pub struct ScoringPolicy {
    pub name: String,
    pub rego_source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_facets: Vec<String>,
}

#[derive(Serialize)]
pub struct ConfigFieldDef {
    pub key: String,
    pub label: String,
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ConfigFieldOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help_text: Option<String>,
}

#[derive(Serialize)]
pub struct ConfigFieldOption {
    pub value: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// Search request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TaggedAlias {
    pub name: String,
    pub language: String,
}

#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub tvdb_id: Option<String>,
    /// Semantic category hint from the caller (e.g. "movie", "tv", "anime").
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub episode: Option<u32>,
    #[serde(default)]
    pub tagged_aliases: Vec<TaggedAlias>,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_current: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grab_current: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grab_max: Option<u32>,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grabs: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

pub struct NewznabConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_path: String,
    pub additional_params: String,
    /// Maximum results the indexer returns per page. Defaults to 200.
    pub page_size: usize,
}

impl NewznabConfig {
    /// Read configuration from Extism host config keys.
    pub fn from_extism() -> Result<Self, Error> {
        let base_url = config::get("base_url")
            .map_err(|e| Error::msg(format!("missing config base_url: {e}")))?
            .unwrap_or_default();
        let api_key = config::get("api_key")
            .map_err(|e| Error::msg(format!("missing config api_key: {e}")))?
            .unwrap_or_default();
        let api_path = config::get("api_path")
            .ok()
            .flatten()
            .unwrap_or_else(|| "/api".to_string());
        let additional_params = config::get("additional_params")
            .ok()
            .flatten()
            .unwrap_or_default();

        if base_url.is_empty() || api_key.is_empty() {
            return Err(Error::msg(
                "Newznab indexer requires base_url and api_key configuration",
            ));
        }

        let page_size = config::get("page_size")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(200)
            .clamp(1, 500);
        Ok(Self {
            base_url,
            api_key,
            api_path,
            additional_params,
            page_size,
        })
    }
}

/// Returns the standard config field declarations for the generic newznab plugin.
pub fn standard_config_fields() -> Vec<ConfigFieldDef> {
    vec![
        ConfigFieldDef {
            key: "api_path".to_string(),
            label: "API Path".to_string(),
            field_type: "string".to_string(),
            required: false,
            default_value: Some("/api".to_string()),
            options: vec![],
            help_text: Some(
                "API endpoint path (e.g. /api, /api/v1/api, /nabapi)".to_string(),
            ),
        },
        ConfigFieldDef {
            key: "additional_params".to_string(),
            label: "Additional Parameters".to_string(),
            field_type: "string".to_string(),
            required: false,
            default_value: None,
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

/// Default extractor for generic Newznab indexers: extracts only `grabs` and `language`.
pub fn extract_base_metadata(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut grabs = None;
    let mut languages = Vec::new();

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
            _ => {}
        }
    }

    (languages, grabs, HashMap::new())
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
    let query_variants = build_query_variants(req, &query);

    let imdb_id = req
        .imdb_id
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let tvdb_id = req
        .tvdb_id
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    if query.is_empty() && imdb_id.is_none() && tvdb_id.is_none() {
        return Ok(SearchResponse {
            results: vec![],
            api_current: None,
            api_max: None,
            grab_current: None,
            grab_max: None,
        });
    }

    // Determine search type from categories and hints
    let search_type = determine_search_type(
        &req.categories,
        req.category.as_deref(),
        imdb_id.as_deref(),
        tvdb_id.as_deref(),
    );

    // Build Newznab category parameter (numeric codes only)
    let newznab_cat = build_category_param(&req.categories);

    let endpoint = build_endpoint(&config.base_url, &config.api_path);

    // Paginated search: fetch up to MAX_PAGES pages.
    // Stop early if a page returns fewer results than the page size.
    let page_size = config.page_size;
    const MAX_PAGES: usize = 5;
    let max_results = page_size * MAX_PAGES;
    let limit = req.limit.clamp(1, max_results);

    let mut all_results: Vec<SearchResult> = Vec::new();
    let mut last_limits = ApiLimits::default();

    for page in 0..MAX_PAGES {
        let offset = page * page_size;
        let page_params = if config.additional_params.is_empty() {
            format!("&offset={offset}")
        } else {
            format!("{}&offset={offset}", config.additional_params)
        };

        let (status, body) = execute_tiered_search(
            &endpoint,
            &search_type,
            &query_variants,
            &config.api_key,
            imdb_id.as_deref(),
            tvdb_id.as_deref(),
            newznab_cat.as_deref(),
            page_size,
            req.season,
            req.episode,
            &page_params,
        )?;

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
// Search type determination
// ---------------------------------------------------------------------------

fn determine_search_type(
    categories: &[String],
    category_hint: Option<&str>,
    imdb_id: Option<&str>,
    tvdb_id: Option<&str>,
) -> String {
    let cats_movie = categories.iter().any(|c| c.starts_with('2'));
    let cats_tv = categories.iter().any(|c| c.starts_with('5'));

    let hint = category_hint.unwrap_or("").to_ascii_lowercase();
    let hint_movie = matches!(hint.as_str(), "movie" | "movies");
    let hint_tv = matches!(hint.as_str(), "tv" | "series" | "anime");

    if cats_movie {
        "movie".to_string()
    } else if cats_tv {
        "tvsearch".to_string()
    } else if hint_movie {
        "movie".to_string()
    } else if hint_tv {
        "tvsearch".to_string()
    } else if imdb_id.is_some() {
        "movie".to_string()
    } else if tvdb_id.is_some() {
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

fn build_query_variants(req: &SearchRequest, query: &str) -> Vec<String> {
    let mut variants = Vec::new();

    if is_anime_category(req.category.as_deref()) {
        if let Some(preferred) = preferred_anime_query(query, &req.tagged_aliases) {
            if !preferred.is_empty() {
                variants.push(preferred);
            }
        }
    }

    if !query.is_empty() {
        variants.push(query.to_string());
    }

    let mut seen = std::collections::HashSet::new();
    variants.retain(|value| seen.insert(value.to_ascii_lowercase()));
    variants
}

fn is_anime_category(category_hint: Option<&str>) -> bool {
    category_hint
        .map(|value| value.trim().eq_ignore_ascii_case("anime"))
        .unwrap_or(false)
}

fn preferred_anime_query(query: &str, tagged_aliases: &[TaggedAlias]) -> Option<String> {
    let alias = tagged_aliases
        .iter()
        .find(|alias| alias.language.eq_ignore_ascii_case("jpn") && is_romanized_alias(&alias.name))?
        .name
        .trim();

    if alias.is_empty() {
        return None;
    }

    let suffix = extract_query_suffix(query);
    if suffix.is_empty() {
        Some(alias.to_string())
    } else {
        Some(format!("{alias} {suffix}"))
    }
}

fn is_romanized_alias(alias: &str) -> bool {
    let trimmed = alias.trim();
    !trimmed.is_empty()
        && trimmed.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, ' ' | '-' | '_' | ':' | ';' | ',' | '.' | '\'' | '&' | '!' | '?')
        })
}

fn extract_query_suffix(query: &str) -> String {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }

    let mut start = tokens.len();
    for index in (0..tokens.len()).rev() {
        if looks_like_context_token(tokens[index]) {
            start = index;
        } else if start != tokens.len() {
            break;
        }
    }

    if start < tokens.len() {
        tokens[start..].join(" ")
    } else {
        String::new()
    }
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

    if upper.starts_with('S') {
        let rest = &upper[1..];
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
    tvdb_id: Option<&str>,
    cat: Option<&str>,
    limit: usize,
    season: Option<u32>,
    episode: Option<u32>,
    additional_params: &str,
) -> Result<(u16, String), Error> {
    // Determine effective IDs for the search type:
    // t=movie doesn't support tvdbid; t=tvsearch doesn't support imdbid
    let effective_imdb = if search_type == "movie" || search_type == "search" {
        imdb_id
    } else {
        None
    };
    let effective_tvdb = if search_type == "tvsearch" || search_type == "search" {
        tvdb_id
    } else {
        None
    };

    let has_id = effective_imdb.is_some() || effective_tvdb.is_some();
    let mut last_query_response: Option<(u16, String)> = None;

    // Tier 1: Query-based search with IDs when query text is available. This
    // matches the documented Newznab/NZBGeek movie and TV search forms.
    for query_text in query_variants.iter().map(String::as_str).filter(|query| !query.is_empty()) {
        let (status, body) = execute_search(
            endpoint,
            search_type,
            Some(query_text),
            api_key,
            effective_imdb,
            effective_tvdb,
            cat,
            limit,
            season,
            episode,
            additional_params,
        )?;

        last_query_response = Some((status, body.clone()));

        if is_success_status(status) {
            let trimmed = body.trim_start();
            let looks_empty = is_empty_response(trimmed);
            if !looks_empty {
                return Ok((status, body));
            }
        }
    }

    if search_type == "search" && !has_id {
        return Ok(last_query_response.unwrap_or((200, r#"{"channel":{}}"#.to_string())));
    }

    // Tier 2: ID-only search when we have an authoritative ID.
    if has_id {
        let (status, body) = execute_search(
            endpoint,
            search_type,
            None,
            api_key,
            effective_imdb,
            effective_tvdb,
            cat,
            limit,
            season,
            episode,
            additional_params,
        )?;

        if is_success_status(status) {
            return Ok((status, body));
        }
    }

    // Tier 3: Generic search fallback (t=search, no IDs, no season/ep)
    if search_type != "search" {
        let mut fallback_queries: Vec<Option<&str>> = query_variants
            .iter()
            .map(String::as_str)
            .filter(|query| !query.is_empty())
            .map(Some)
            .collect();

        if fallback_queries.is_empty() {
            fallback_queries.push(imdb_id.or(tvdb_id).filter(|value| !value.is_empty()));
        }

        let mut last_fallback = (200, r#"{"channel":{}}"#.to_string());
        for fallback_query in fallback_queries {
            let (status, body) = execute_search(
                endpoint,
                "search",
                fallback_query,
                api_key,
                None,
                None,
                cat,
                limit,
                None,
                None,
                additional_params,
            )?;
            let looks_empty = is_empty_response(body.trim_start());
            last_fallback = (status, body.clone());
            if is_success_status(status) && !looks_empty {
                return Ok((status, body));
            }
        }
        return Ok(last_fallback);
    }

    // Shouldn't reach here, but return empty
    Ok((200, r#"{"channel":{}}"#.to_string()))
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

fn build_endpoint(base_url: &str, api_path: &str) -> String {
    let cleaned = base_url.trim_end_matches('/');
    let path = api_path.trim().trim_start_matches('/').trim_end_matches('/');
    if path.is_empty() {
        cleaned.to_string()
    } else {
        format!("{cleaned}/{path}")
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_search(
    endpoint: &str,
    search_type: &str,
    query: Option<&str>,
    api_key: &str,
    imdb_id: Option<&str>,
    tvdb_id: Option<&str>,
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
        tvdb_id,
        cat,
        limit,
        season,
        episode,
        additional_params,
    );

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
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("User-Agent", USER_AGENT);

    let mut next_delay: u64 = 0;
    for attempt in 0..=BACKOFF_SECS.len() {
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

        let resp = http::request::<Vec<u8>>(&http_req, None)
            .map_err(|e| {
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
            if attempt >= BACKOFF_SECS.len() {
                return Err(Error::msg("HTTP 429: rate limited after all retries"));
            }

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
                None => BACKOFF_SECS[attempt],
            };
            continue;
        }

        return Ok((resp.status_code(), String::from_utf8_lossy(&resp.body()).to_string()));
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
    tvdb_id: Option<&str>,
    cat: Option<&str>,
    limit: usize,
    season: Option<u32>,
    episode: Option<u32>,
    additional_params: &str,
) -> String {
    let imdb_id = imdb_id.map(normalize_imdbid_param);
    let mut url = format!(
        "{endpoint}?t={search_type}&apikey={api_key}&o=json&extended=1&limit={limit}"
    );

    if let Some(q) = query {
        url.push_str("&q=");
        url.push_str(&url_encode(q));
    }
    if let Some(id) = imdb_id.as_deref() {
        url.push_str("&imdbid=");
        url.push_str(id);
    }
    if let Some(id) = tvdb_id {
        url.push_str("&tvdbid=");
        url.push_str(id);
    }
    if let Some(c) = cat {
        url.push_str("&cat=");
        url.push_str(c);
    }
    if let Some(s) = season {
        url.push_str(&format!("&season={s}"));
    }
    if let Some(e) = episode {
        url.push_str(&format!("&ep={e}"));
    }
    if !additional_params.is_empty() {
        let params = additional_params.trim();
        if !params.starts_with('&') {
            url.push('&');
        }
        url.push_str(params);
    }

    url
}

fn normalize_imdbid_param(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() > 2
        && trimmed[..2].eq_ignore_ascii_case("tt")
        && trimmed[2..].chars().all(|ch| ch.is_ascii_digit())
    {
        format!("00{}", &trimmed[2..])
    } else {
        trimmed.to_string()
    }
}

/// Minimal percent-encoding for query string values.
fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => output.push(byte as char),
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
            "tvdbid" => {
                if !value.is_empty() && value != "0" {
                    result
                        .extra
                        .insert("response_tvdbid".to_string(), serde_json::Value::from(value.as_str()));
                }
            }
            "imdb" | "imdbid" => {
                if !value.is_empty() && value != "0" {
                    result
                        .extra
                        .insert("response_imdbid".to_string(), serde_json::Value::from(value.as_str()));
                }
            }
            "prematch" | "haspretime" => {
                if value != "0" {
                    let flags = result
                        .extra
                        .entry("indexer_flags".to_string())
                        .or_insert_with(|| serde_json::Value::Array(vec![]));
                    if let serde_json::Value::Array(ref mut arr) = flags {
                        arr.push(serde_json::Value::from("scene"));
                    }
                }
            }
            "nuked" => {
                if value != "0" {
                    let flags = result
                        .extra
                        .entry("indexer_flags".to_string())
                        .or_insert_with(|| serde_json::Value::Array(vec![]));
                    if let serde_json::Value::Array(ref mut arr) = flags {
                        arr.push(serde_json::Value::from("nuked"));
                    }
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
    let attrs = parsed.error?.attributes?;
    let code = attrs.code.unwrap_or_else(|| "unknown".into());
    let description = attrs.description.unwrap_or_else(|| "unknown".into());
    Some((code, description))
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
            let enclosure_type = enclosure_attrs
                .as_ref()
                .and_then(|a| a.mime_type.clone());

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
                extra,
                guid: item.guid,
                info_url: item.comments.as_ref().map(|c| {
                    c.split('#').next().unwrap_or(c).trim().to_string()
                }).filter(|s| !s.is_empty()),
            };

            // Apply standard attrs (usenetdate, prematch, nuked, response IDs)
            let mut usenet_date = None;
            apply_standard_attrs(&pairs, &mut result, &mut usenet_date);

            // Prefer usenetdate over pubDate
            if usenet_date.is_some() {
                result.published_at = usenet_date;
            }

            // Store non-NZB enclosure type as metadata
            if let Some(ref mime) = enclosure_type {
                if mime != "application/x-nzb" {
                    result
                        .extra
                        .insert("enclosure_type".to_string(), serde_json::Value::from(mime.as_str()));
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
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                if e.name().as_ref() == b"error" =>
            {
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
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
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
                                        size_bytes =
                                            v.replace(',', "").parse::<i64>().ok();
                                        break;
                                    }
                                }
                            }

                            // Run provider-specific extractor
                            let (languages, grabs, extra) = extract_fn(&attrs);

                            let info_url = comments.as_ref().map(|c| {
                                c.split('#').next().unwrap_or(c).trim().to_string()
                            }).filter(|s| !s.is_empty());

                            let mut result = SearchResult {
                                title: t.clone(),
                                link: link.clone(),
                                download_url: download_url.clone(),
                                size_bytes,
                                published_at: pub_date.clone(),
                                grabs,
                                languages,
                                extra,
                                guid: guid.clone(),
                                info_url,
                            };

                            // Apply standard attrs
                            let mut usenet_date = None;
                            apply_standard_attrs(&attrs, &mut result, &mut usenet_date);

                            if usenet_date.is_some() {
                                result.published_at = usenet_date;
                            }

                            // Store non-NZB enclosure type
                            if let Some(ref mime) = enclosure_type {
                                if mime != "application/x-nzb" {
                                    result.extra.insert(
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
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"url" => {
                *download_url = String::from_utf8(attr.value.to_vec()).ok();
            }
            b"length" => {
                *size_bytes = String::from_utf8(attr.value.to_vec())
                    .ok()
                    .and_then(|v| v.replace(',', "").parse::<i64>().ok());
            }
            b"type" => {
                *enclosure_type = String::from_utf8(attr.value.to_vec()).ok();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── determine_search_type ────────────────────────────────────────────

    #[test]
    fn search_type_movie_category() {
        let cats = vec!["2000".into()];
        assert_eq!(determine_search_type(&cats, None, None, None), "movie");
    }

    #[test]
    fn search_type_tv_category() {
        let cats = vec!["5000".into()];
        assert_eq!(determine_search_type(&cats, None, None, None), "tvsearch");
    }

    #[test]
    fn search_type_movie_hint() {
        assert_eq!(
            determine_search_type(&[], Some("movie"), None, None),
            "movie"
        );
    }

    #[test]
    fn search_type_tv_hint() {
        assert_eq!(
            determine_search_type(&[], Some("tv"), None, None),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_anime_hint() {
        assert_eq!(
            determine_search_type(&[], Some("anime"), None, None),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_imdb_id_fallback() {
        assert_eq!(
            determine_search_type(&[], None, Some("1234567"), None),
            "movie"
        );
    }

    #[test]
    fn search_type_tvdb_id_fallback() {
        assert_eq!(
            determine_search_type(&[], None, None, Some("12345")),
            "tvsearch"
        );
    }

    #[test]
    fn search_type_generic_fallback() {
        assert_eq!(determine_search_type(&[], None, None, None), "search");
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
            200,
            None,
            None,
            "",
        );

        assert!(url.contains("t=movie"));
        assert!(url.contains("q=12%20years%20a%20slave"));
        assert!(url.contains("imdbid=002024544"));
        assert!(url.contains("limit=200"));
        assert!(url.contains("extended=1"));
        assert!(url.contains("o=json"));
    }

    #[test]
    fn build_search_url_tv_prefers_query_and_tvdbid() {
        let url = build_search_url(
            "https://api.nzbgeek.info/api",
            "tvsearch",
            Some("demon slayer"),
            "test-api-key",
            None,
            Some("123456"),
            None,
            200,
            None,
            None,
            "",
        );

        assert!(url.contains("t=tvsearch"));
        assert!(url.contains("q=demon%20slayer"));
        assert!(url.contains("tvdbid=123456"));
        assert!(url.contains("limit=200"));
        assert!(url.contains("extended=1"));
        assert!(url.contains("o=json"));
    }

    #[test]
    fn preferred_anime_query_uses_romanized_jpn_alias_with_episode_suffix() {
        let query = preferred_anime_query(
            "Frieren S01E01",
            &[
                TaggedAlias {
                    name: "葬送のフリーレン".into(),
                    language: "jpn".into(),
                },
                TaggedAlias {
                    name: "Sousou no Frieren".into(),
                    language: "jpn".into(),
                },
            ],
        );

        assert_eq!(query.as_deref(), Some("Sousou no Frieren S01E01"));
    }

    #[test]
    fn build_query_variants_prefers_romanized_anime_alias_before_canonical() {
        let req = SearchRequest {
            query: "Frieren S01E01".into(),
            imdb_id: None,
            tvdb_id: Some("424536".into()),
            category: Some("anime".into()),
            categories: vec![],
            limit: 100,
            season: Some(1),
            episode: Some(1),
            tagged_aliases: vec![TaggedAlias {
                name: "Sousou no Frieren".into(),
                language: "jpn".into(),
            }],
        };

        let variants = build_query_variants(&req, &req.query);
        assert_eq!(variants[0], "Sousou no Frieren S01E01");
        assert_eq!(variants[1], "Frieren S01E01");
    }

    #[test]
    fn redact_url_for_log_redacts_apikey() {
        let redacted = redact_url_for_log(
            "https://example.test/api?t=movie&apikey=secret&o=json&token=abc",
        );
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
    fn normalize_imdbid_rewrites_tt_prefix_to_double_zero() {
        assert_eq!(normalize_imdbid_param("tt2024544"), "002024544");
        assert_eq!(normalize_imdbid_param("TT1234567"), "001234567");
        assert_eq!(normalize_imdbid_param("1234567"), "1234567");
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
            build_endpoint("https://api.nzbgeek.info", "/api"),
            "https://api.nzbgeek.info/api"
        );
    }

    #[test]
    fn endpoint_trailing_slash() {
        assert_eq!(
            build_endpoint("https://example.com/", "/api/"),
            "https://example.com/api"
        );
    }

    #[test]
    fn endpoint_empty_path() {
        assert_eq!(
            build_endpoint("https://example.com", ""),
            "https://example.com"
        );
    }

    #[test]
    fn endpoint_custom_path() {
        assert_eq!(
            build_endpoint("https://foo.bar", "/api/v1/api"),
            "https://foo.bar/api/v1/api"
        );
    }

    // ── is_empty_response ────────────────────────────────────────────────

    #[test]
    fn empty_json_no_title() {
        assert!(is_empty_response(r#"{"channel":{}}"#));
    }

    #[test]
    fn non_empty_json() {
        assert!(!is_empty_response(r#"{"channel":{"item":{"title":"foo"}}}"#));
    }

    #[test]
    fn empty_xml_no_item() {
        assert!(is_empty_response("<rss><channel></channel></rss>"));
    }

    #[test]
    fn non_empty_xml() {
        assert!(!is_empty_response("<rss><channel><item><title>foo</title></item></channel></rss>"));
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
            extra: HashMap::new(),
            guid: None,
            info_url: None,
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
            result.extra.get("response_tvdbid"),
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
            result.extra.get("response_imdbid"),
            Some(&serde_json::Value::from("1234567"))
        );
    }

    #[test]
    fn attrs_prematch() {
        let pairs = vec![("prematch".into(), "1".into())];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        let flags = result.extra.get("indexer_flags").unwrap();
        assert!(flags.as_array().unwrap().contains(&serde_json::Value::from("scene")));
    }

    #[test]
    fn attrs_nuked() {
        let pairs = vec![("nuked".into(), "1".into())];
        let mut result = make_result();
        let mut usenet_date = None;
        apply_standard_attrs(&pairs, &mut result, &mut usenet_date);
        let flags = result.extra.get("indexer_flags").unwrap();
        assert!(flags.as_array().unwrap().contains(&serde_json::Value::from("nuked")));
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
        assert!(result.extra.is_empty(), "got: {:?}", result.extra);
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
        assert_eq!(r.download_url.as_deref(), Some("https://example.com/download/abc123"));
        assert_eq!(r.size_bytes, Some(1_073_741_824));
        assert_eq!(r.grabs, Some(42));
        assert_eq!(r.languages, vec!["English"]);
        assert_eq!(r.info_url.as_deref(), Some("https://example.com/details/abc123"));
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
        assert_eq!(r.download_url.as_deref(), Some("https://example.com/dl/def456"));
        assert_eq!(r.size_bytes, Some(2_147_483_648));
        assert_eq!(r.grabs, Some(99));
        assert_eq!(r.languages, vec!["English", "French"]);
        assert_eq!(r.info_url.as_deref(), Some("https://example.com/details/def456"));
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

    // ── standard_config_fields ───────────────────────────────────────────

    #[test]
    fn config_fields_has_api_path_and_additional_params() {
        let fields = standard_config_fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].key, "api_path");
        assert_eq!(fields[1].key, "additional_params");
    }
}
