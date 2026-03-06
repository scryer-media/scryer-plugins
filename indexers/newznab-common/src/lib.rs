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
}

#[derive(Serialize)]
pub struct Capabilities {
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

        Ok(Self {
            base_url,
            api_key,
            api_path,
            additional_params,
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
/// 3. Executes a tiered search (ID-based → query-based → generic fallback)
/// 4. Auto-detects response format (JSON or XML) and parses
/// 5. Classifies errors with Newznab-specific handling
pub fn execute_full_search(
    config: &NewznabConfig,
    req: &SearchRequest,
    extract_fn: MetadataExtractor,
) -> Result<SearchResponse, Error> {
    let query = req.query.trim().to_string();
    let limit = req.limit.clamp(1, 200);

    let imdb_id = req
        .imdb_id
        .as_ref()
        .map(|v| v.trim().trim_start_matches("tt").to_string())
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

    // Tiered search execution
    let (status, body) = execute_tiered_search(
        &endpoint,
        &search_type,
        &query,
        &config.api_key,
        imdb_id.as_deref(),
        tvdb_id.as_deref(),
        newznab_cat.as_deref(),
        limit,
        req.season,
        req.episode,
        &config.additional_params,
    )?;

    // Detect response format and check for errors
    let trimmed = body.trim_start();
    let is_xml = trimmed.starts_with("<?xml")
        || trimmed.starts_with("<rss")
        || trimmed.starts_with("<error");

    if is_xml {
        if let Some((code, description)) = parse_error_xml(&body) {
            return Err(classify_and_format_error(&code, &description));
        }
    } else if let Some((code, description)) = parse_error_json(&body) {
        return Err(classify_and_format_error(&code, &description));
    }

    if status >= 400 {
        return Err(Error::msg(format!("Newznab API returned HTTP {status}")));
    }

    let (results, limits) = if is_xml {
        parse_newznab_xml(&body, limit, extract_fn)
    } else {
        parse_newznab_json(&body, limit, extract_fn)
    };

    Ok(SearchResponse {
        results,
        api_current: limits.api_current,
        api_max: limits.api_max,
        grab_current: limits.grab_current,
        grab_max: limits.grab_max,
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

// ---------------------------------------------------------------------------
// Tiered search execution
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_tiered_search(
    endpoint: &str,
    search_type: &str,
    query: &str,
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

    // Tier 1: ID-based search (no &q=) when we have an ID
    if has_id {
        let (status, body) = execute_search(
            endpoint,
            search_type,
            None, // no query text — ID is authoritative
            api_key,
            effective_imdb,
            effective_tvdb,
            cat,
            limit,
            season,
            episode,
            additional_params,
        )?;

        // If tier 1 succeeded (non-5xx), return it
        if status < 500 {
            // Check if we got an empty result with IDs — try adding query text
            let trimmed = body.trim_start();
            let looks_empty = is_empty_response(trimmed);

            if !looks_empty {
                return Ok((status, body));
            }
            // Fall through to tier 2 with query text
        }
    }

    // Tier 2: Query-based search with search type
    let effective_query = if query.is_empty() { None } else { Some(query) };
    if effective_query.is_some() || has_id {
        let (status, body) = execute_search(
            endpoint,
            search_type,
            effective_query,
            api_key,
            effective_imdb,
            effective_tvdb,
            cat,
            limit,
            season,
            episode,
            additional_params,
        )?;

        if status < 500 {
            return Ok((status, body));
        }
    }

    // Tier 3: Generic search fallback (t=search, no IDs, no season/ep)
    if search_type != "search" {
        let fallback_query = if query.is_empty() {
            imdb_id.or(tvdb_id)
        } else {
            Some(query)
        }
        .filter(|v| !v.is_empty());

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
        return Ok((status, body));
    }

    // Shouldn't reach here, but return empty
    Ok((200, r#"{"channel":{}}"#.to_string()))
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
    let mut url = format!(
        "{endpoint}?t={search_type}&apikey={api_key}&o=json&extended=1&limit={limit}"
    );

    if let Some(q) = query {
        url.push_str("&q=");
        url.push_str(&url_encode(q));
    }
    if let Some(id) = imdb_id {
        url.push_str("&imdbid=");
        url.push_str(id);
    }
    if let Some(id) = tvdb_id {
        url.push_str("&tvdbid=");
        url.push_str(id);
    }
    // Aggregate ID search: include both when available
    if imdb_id.is_some() && tvdb_id.is_some() {
        // Already appended above — both are included
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

    let http_req = HttpRequest::new(&url)
        .with_header("Accept", "application/json, application/xml, */*; q=0.8")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
        );

    let resp = http::request::<Vec<u8>>(&http_req, None)
        .map_err(|e| Error::msg(format!("HTTP request failed: {e}")))?;

    let status = resp.status_code();
    let body = String::from_utf8_lossy(&resp.body()).to_string();

    Ok((status, body))
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
