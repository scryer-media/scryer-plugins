use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use extism_pdk::*;
use newznab_common::{
    NewznabConfig, NewznabHitBudget, NewznabHttpBehavior, SearchRequest, SearchResult,
    current_sdk_constraint, execute_raw_search, is_hit_budget_exhausted_error,
};
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor,
    PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
    SubtitleCapabilities, SubtitleDescriptor, SubtitleMatchHint, SubtitleMatchHintKind,
    SubtitlePluginCandidate, SubtitlePluginDownloadRequest, SubtitlePluginDownloadResponse,
    SubtitlePluginSearchRequest, SubtitlePluginSearchResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

const PROVIDER_ID: &str = "amenzb-subtitles";
const PROVIDER_TYPE: &str = "amenzb";
const DEFAULT_BASE_URL: &str = "https://amenzb.moe";
const DEFAULT_API_PATH: &str = "/api";
const DEFAULT_CATEGORY: &str = "5070";
const DEFAULT_MAX_RESULTS: usize = 20;
const DEFAULT_MAX_DETAIL_FETCHES: usize = 10;
const API_MAX_RESULTS: usize = 100;
const DEFAULT_DAILY_HIT_CAP: u32 = 9_000;
const MAX_REDIRECTS: usize = 3;
const MAX_SUBTITLE_BYTES: usize = 2 * 1024 * 1024;
const RATE_LIMIT_BACKOFF_SECONDS: u64 = 30;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_ID.to_string(),
        name: "ameNZB Subtitles".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec!["amenzb.moe".to_string()],
            config_fields: config_fields(),
            default_base_url: Some(DEFAULT_BASE_URL.to_string()),
            allowed_hosts: vec![],
            capabilities: SubtitleCapabilities {
                mode: SubtitleProviderMode::Catalog,
                supported_media_kinds: vec![SubtitleQueryMediaKind::Episode],
                recommended_facets: vec!["anime".to_string()],
                supports_hash_lookup: false,
                supports_forced: false,
                supports_hearing_impaired: false,
                supports_ai_translated: false,
                supports_machine_translated: false,
                supported_languages: vec![],
            },
        }),
    }
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    let response = match AmenzbConfig::from_extism() {
        Ok(config) if config.api_key.trim().is_empty() => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::InvalidConfig,
            message: Some("ameNZB API key is required".to_string()),
            retry_after_seconds: None,
        },
        Ok(_) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Valid,
            message: None,
            retry_after_seconds: None,
        },
        Err(error) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::InvalidConfig,
            message: Some(error),
            retry_after_seconds: None,
        },
    };
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_search(input: String) -> FnResult<String> {
    let request: SubtitlePluginSearchRequest = serde_json::from_str(&input)?;
    let config = AmenzbConfig::from_extism().map_err(Error::msg)?;
    let response = match subtitle_search_impl(&config, &request) {
        Ok(results) => SubtitlePluginSearchResponse { results },
        Err(AmenzbError::RateLimited(_)) => SubtitlePluginSearchResponse::default(),
        Err(error) => return Err(Error::msg(error.to_string()).into()),
    };
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_download(input: String) -> FnResult<String> {
    let request: SubtitlePluginDownloadRequest = serde_json::from_str(&input)?;
    let config = AmenzbConfig::from_extism().map_err(Error::msg)?;
    let reference: AmenzbDownloadRef =
        serde_json::from_str(&request.provider_file_id).map_err(Error::msg)?;
    let result = match subtitle_download_impl(&config, &reference) {
        Ok(response) => PluginResult::Ok(response),
        Err(error) => PluginResult::Err(plugin_error(error)),
    };
    Ok(serde_json::to_string(&result)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "base_url",
            "Base URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("ameNZB site URL"),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("ameNZB API key. Required for Newznab search; keys are IP-pinned by ameNZB."),
        ),
        field(
            "api_path",
            "API Path",
            ConfigFieldType::String,
            false,
            Some(DEFAULT_API_PATH.to_string()),
            Some("Newznab API endpoint path"),
        ),
        field(
            "max_results",
            "Max Results",
            ConfigFieldType::Number,
            false,
            Some(DEFAULT_MAX_RESULTS.to_string()),
            Some("Default search result count; hard-capped at 100"),
        ),
        field(
            "max_detail_fetches",
            "Max Detail Fetches",
            ConfigFieldType::Number,
            false,
            Some(DEFAULT_MAX_DETAIL_FETCHES.to_string()),
            Some("Maximum release pages fetched during one subtitle search"),
        ),
        field(
            "category",
            "Category",
            ConfigFieldType::String,
            false,
            Some(DEFAULT_CATEGORY.to_string()),
            Some("Default Newznab category. 5070 is anime."),
        ),
        field(
            "healthy_only",
            "Healthy Only",
            ConfigFieldType::Bool,
            false,
            Some("false".to_string()),
            Some("Send healthy=1 to filter for releases ameNZB considers healthy"),
        ),
        field(
            "hourly_hit_cap",
            "Hourly Hit Cap",
            ConfigFieldType::Number,
            false,
            Some("450".to_string()),
            Some("Maximum ameNZB API requests per hour before searches return no results"),
        ),
        field(
            "daily_hit_cap",
            "Daily Hit Cap",
            ConfigFieldType::Number,
            false,
            Some(DEFAULT_DAILY_HIT_CAP.to_string()),
            Some("Maximum ameNZB API requests per day before searches return no results"),
        ),
    ]
}

#[derive(Debug, Clone)]
struct AmenzbConfig {
    base_url: String,
    api_key: String,
    api_path: String,
    max_results: usize,
    max_detail_fetches: usize,
    category: Option<String>,
    healthy_only: bool,
}

impl AmenzbConfig {
    fn from_extism() -> Result<Self, String> {
        Ok(Self {
            base_url: config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            api_key: config_value("api_key").unwrap_or_default(),
            api_path: config_value("api_path").unwrap_or_else(|| DEFAULT_API_PATH.to_string()),
            max_results: config_usize("max_results", DEFAULT_MAX_RESULTS).clamp(1, API_MAX_RESULTS),
            max_detail_fetches: config_usize("max_detail_fetches", DEFAULT_MAX_DETAIL_FETCHES),
            category: config_value("category").or_else(|| Some(DEFAULT_CATEGORY.to_string())),
            healthy_only: config_bool("healthy_only", false),
        })
    }

    fn site_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    fn newznab_config(&self, request: &SubtitlePluginSearchRequest) -> NewznabConfig {
        let mut config = NewznabConfig {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            api_path: self.api_path.clone(),
            additional_params: provider_params(self, request),
            page_size: self.max_results,
            http_behavior: NewznabHttpBehavior::default(),
        };
        apply_http_behavior(&mut config);
        config
    }

    fn http_behavior(&self) -> NewznabHttpBehavior {
        let mut config = NewznabConfig {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            api_path: self.api_path.clone(),
            additional_params: String::new(),
            page_size: self.max_results,
            http_behavior: NewznabHttpBehavior::default(),
        };
        apply_http_behavior(&mut config);
        config.http_behavior
    }
}

fn apply_http_behavior(config: &mut NewznabConfig) {
    config.http_behavior = NewznabHttpBehavior {
        plugin_id: PROVIDER_ID.to_string(),
        user_agent: USER_AGENT.to_string(),
        pre_request_delay: Duration::from_millis(250),
        retry_total_budget: Duration::from_secs(30),
        retry_default_delay: Duration::from_secs(RATE_LIMIT_BACKOFF_SECONDS),
        retry_max_delay: Duration::from_secs(120),
        retry_max_attempts: 2,
        max_search_pages: 1,
        hit_budget: Some(NewznabHitBudget {
            var_key: "amenzb_subtitles.http_hits".to_string(),
            hourly_limit: config_u32("hourly_hit_cap", 450),
            daily_limit: config_u32("daily_hit_cap", DEFAULT_DAILY_HIT_CAP),
        }),
    };
}

fn subtitle_search_impl(
    config: &AmenzbConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, AmenzbError> {
    if request.media_kind != SubtitleQueryMediaKind::Episode {
        return Ok(vec![]);
    }

    let search_request = search_request_for(config, request);

    let response = execute_raw_search(
        &config.newznab_config(request),
        &search_request,
        amenzb_metadata_extractor,
    )
    .map_err(AmenzbError::from_search_error)?;

    let requested_languages = requested_languages(&request.languages);
    let behavior = config.http_behavior();
    let mut results = Vec::new();
    let mut detail_fetches = 0usize;

    for release in response.results {
        if detail_fetches >= config.max_detail_fetches || results.len() >= config.max_results {
            break;
        }
        if !release_matches_requested_languages(&release, &requested_languages) {
            continue;
        }
        let Some(release_id) = release_id(&release) else {
            continue;
        };
        detail_fetches += 1;
        let url = format!("{}/release/{release_id}", config.site_url());
        let page = match http_get_follow(&url, "text/html, */*", &behavior) {
            Ok(page) => page,
            Err(AmenzbError::RateLimited(_)) => return Ok(results),
            Err(error) => return Err(error),
        };
        if page.status >= 400 {
            if page.status == 429 {
                return Err(AmenzbError::RateLimited(None));
            }
            continue;
        }
        for link in parse_subtitle_links(&page.body, config.site_url()) {
            if !requested_language_matches(&requested_languages, &link.language) {
                continue;
            }
            results.push(candidate_for_link(request, &release, link)?);
            if results.len() >= config.max_results {
                break;
            }
        }
    }

    Ok(results)
}

fn search_request_for(
    config: &AmenzbConfig,
    request: &SubtitlePluginSearchRequest,
) -> SearchRequest {
    let query = if has_exact_provider_filter(request) {
        String::new()
    } else {
        search_query(request)
    };
    let categories = config
        .category
        .as_ref()
        .map(|category| vec![category.clone()])
        .unwrap_or_default();
    SearchRequest {
        query,
        ids: HashMap::new(),
        facet: Some("anime".to_string()),
        category: Some("anime".to_string()),
        categories,
        limit: config.max_results,
        season: request.season.and_then(i32_to_u32),
        episode: request.episode.and_then(i32_to_u32),
        absolute_episode: request.absolute_episode.and_then(i32_to_u32),
        tagged_aliases: vec![],
        context: None,
    }
}

fn subtitle_download_impl(
    config: &AmenzbConfig,
    reference: &AmenzbDownloadRef,
) -> Result<SubtitlePluginDownloadResponse, AmenzbError> {
    validate_download_url(config, &reference.url)?;
    let response = http_get_follow_checked(
        &reference.url,
        "application/octet-stream, text/plain, */*",
        &config.http_behavior(),
        |url| validate_download_url(config, url),
    )?;
    if response.status == 429 {
        return Err(AmenzbError::RateLimited(retry_after_seconds(&response)));
    }
    if response.status >= 400 {
        return Err(AmenzbError::Message(format!(
            "ameNZB subtitle download returned HTTP {}",
            response.status
        )));
    }
    let bytes = response.body.as_bytes();
    if bytes.len() > MAX_SUBTITLE_BYTES {
        return Err(AmenzbError::Message(format!(
            "ameNZB subtitle is too large ({} bytes)",
            bytes.len()
        )));
    }
    let filename = content_disposition_filename(&response)
        .or_else(|| Some(reference.filename.clone()))
        .filter(|filename| !filename.trim().is_empty());
    let format = resolve_download_format(filename.as_deref(), &response.body, &reference.format);

    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(bytes),
        format,
        filename,
        content_type: header_value(&response, "content-type")
            .or_else(|| Some("application/octet-stream".to_string())),
    })
}

fn provider_params(config: &AmenzbConfig, request: &SubtitlePluginSearchRequest) -> String {
    let mut pairs = Vec::new();
    if config.healthy_only {
        pairs.push(("healthy".to_string(), "1".to_string()));
    }
    if let Some(info_hash) = external_info_hash(request) {
        pairs.push(("info_hash".to_string(), info_hash.to_ascii_lowercase()));
    }
    if let Some(anidb_id) =
        external_id(request, "anidb_id").or_else(|| external_id(request, "anidb"))
    {
        pairs.push(("anime_id".to_string(), anidb_id));
        if let Some(season) = request.season.and_then(i32_to_u32) {
            pairs.push(("season".to_string(), season.to_string()));
        }
        if let Some(episode) = request
            .absolute_episode
            .or(request.episode)
            .and_then(i32_to_u32)
        {
            pairs.push(("ep".to_string(), episode.to_string()));
        }
    }
    if let Some(language) = single_requested_language(&request.languages) {
        pairs.push((
            "sub_lang".to_string(),
            amenzb_language(&language).to_string(),
        ));
    }
    if let Some(source) = request
        .source
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        pairs.push(("source".to_string(), source.to_string()));
    }
    if let Some(resolution) = request
        .resolution
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        pairs.push(("resolution".to_string(), resolution.to_string()));
    }
    if let Some(group) = request
        .release_group
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        pairs.push(("release_group".to_string(), group.to_string()));
    }
    encode_query_pairs(pairs)
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
            "guid" | "season" | "episode" | "source" | "resolution" | "releasegroup"
                if !value.trim().is_empty() =>
            {
                extra.insert(normalized, json!(value.trim()));
            }
            _ => {}
        }
    }
    (languages, grabs, extra)
}

#[derive(Debug, Clone)]
struct SubtitleLink {
    url: String,
    subtitle_id: String,
    release_id: String,
    language: String,
    label: String,
    row_format: String,
    size_label: Option<String>,
    default_track: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AmenzbDownloadRef {
    url: String,
    release_id: String,
    subtitle_id: String,
    filename: String,
    language: String,
    format: String,
    label: String,
}

fn parse_subtitle_links(html: &str, site_url: &str) -> Vec<SubtitleLink> {
    let Some(section_start) = html.find("id=\"subtitlesBody\"") else {
        return vec![];
    };
    let section = &html[section_start..];
    let section_end = section
        .find("</table>")
        .map(|offset| offset + "</table>".len())
        .unwrap_or(section.len());
    let table = &section[..section_end];
    let mut links = Vec::new();
    let mut cursor = 0;
    while let Some(row_offset) = table[cursor..].find("<tr") {
        let row_start = cursor + row_offset;
        let Some(row_end_offset) = table[row_start..].find("</tr>") else {
            break;
        };
        let row_end = row_start + row_end_offset + "</tr>".len();
        let row = &table[row_start..row_end];
        if let Some(link) = parse_subtitle_row(row, site_url) {
            links.push(link);
        }
        cursor = row_end;
    }
    links
}

fn parse_subtitle_row(row: &str, site_url: &str) -> Option<SubtitleLink> {
    let href = attr_value(row, "href")?;
    if !href.contains("/subtitles/") {
        return None;
    }
    let cells = table_cells(row);
    if cells.len() < 4 {
        return None;
    }
    let language = text_content(&cells[0]);
    let label = text_content(&cells[1]);
    let row_format = text_content(&cells[2]).to_ascii_lowercase();
    let size_label = Some(text_content(&cells[3])).filter(|value| !value.is_empty());
    let (release_id, subtitle_id) = subtitle_path_ids(&href)?;
    Some(SubtitleLink {
        url: absolutize_url(site_url, &href),
        subtitle_id,
        release_id,
        language: language_to_scryer(&language).to_string(),
        label,
        row_format,
        size_label,
        default_track: row.contains("Default"),
    })
}

fn candidate_for_link(
    request: &SubtitlePluginSearchRequest,
    release: &SearchResult,
    link: SubtitleLink,
) -> Result<SubtitlePluginCandidate, AmenzbError> {
    let format = normalized_row_format(&link)
        .or_else(|| format_from_label(&link.label))
        .unwrap_or_else(|| "txt".to_string());
    let filename = format!("amenzb-{}-{}.{}", link.release_id, link.subtitle_id, format);
    let provider_file_id = serde_json::to_string(&AmenzbDownloadRef {
        url: link.url.clone(),
        release_id: link.release_id.clone(),
        subtitle_id: link.subtitle_id.clone(),
        filename,
        language: link.language.clone(),
        format: format.clone(),
        label: link.label.clone(),
    })
    .map_err(|error| {
        AmenzbError::Message(format!("failed to encode ameNZB subtitle ref: {error}"))
    })?;

    let mut match_hints = release_match_hints(request);
    match_hints.push(SubtitleMatchHint {
        kind: SubtitleMatchHintKind::Language,
        value: Some(link.language.clone()),
    });
    let track_label = if link.default_track {
        format!("{} (default)", link.label)
    } else {
        link.label.clone()
    };
    let release_info = if let Some(size) = link.size_label.as_deref() {
        Some(format!("{} / {track_label} / {size}", release.title))
    } else {
        Some(format!("{} / {track_label}", release.title))
    };

    Ok(SubtitlePluginCandidate {
        provider_file_id,
        language: link.language,
        release_info,
        hearing_impaired: false,
        forced: false,
        ai_translated: false,
        machine_translated: false,
        uploader: release
            .provider_extra
            .get("releasegroup")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        download_count: release.grabs,
        match_hints,
    })
}

fn search_query(request: &SubtitlePluginSearchRequest) -> String {
    request
        .title_candidates
        .iter()
        .chain(request.title_aliases.iter())
        .find(|candidate| !candidate.trim().is_empty())
        .map(|candidate| candidate.trim().to_string())
        .unwrap_or_else(|| request.title.trim().to_string())
}

fn release_id(release: &SearchResult) -> Option<String> {
    release
        .provider_extra
        .get("guid")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            release
                .guid
                .as_deref()
                .and_then(|guid| guid.rsplit('/').next())
                .map(ToString::to_string)
        })
        .filter(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()))
}

fn release_matches_requested_languages(release: &SearchResult, requested: &[String]) -> bool {
    if requested.is_empty() {
        return true;
    }
    let subtitles = release
        .provider_extra
        .get("subtitle_languages")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(language_to_scryer)
        .collect::<Vec<_>>();
    subtitles.is_empty()
        || subtitles
            .iter()
            .any(|language| requested.iter().any(|req| language_matches(req, language)))
}

fn requested_language_matches(requested: &[String], language: &str) -> bool {
    requested.is_empty()
        || requested
            .iter()
            .any(|requested| language_matches(requested, language))
}

fn language_matches(requested: &str, actual: &str) -> bool {
    let requested = language_to_scryer(requested);
    let actual = language_to_scryer(actual);
    requested == actual || actual == "und"
}

fn requested_languages(languages: &[String]) -> Vec<String> {
    languages
        .iter()
        .map(|language| language_to_scryer(language).to_string())
        .collect()
}

fn release_match_hints(request: &SubtitlePluginSearchRequest) -> Vec<SubtitleMatchHint> {
    vec![
        SubtitleMatchHint {
            kind: SubtitleMatchHintKind::ExternalId,
            value: external_id(request, "anidb_id")
                .or_else(|| external_id(request, "anidb"))
                .map(|value| format!("anidb:{value}")),
        },
        SubtitleMatchHint {
            kind: SubtitleMatchHintKind::AbsoluteEpisode,
            value: request.absolute_episode.map(|episode| episode.to_string()),
        },
        SubtitleMatchHint {
            kind: SubtitleMatchHintKind::SeasonEpisode,
            value: request
                .season
                .zip(request.episode)
                .map(|(season, episode)| format!("S{season:02}E{episode:02}")),
        },
    ]
}

#[derive(Debug, Clone)]
struct HttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

fn http_get_follow(
    url: &str,
    accept: &str,
    behavior: &NewznabHttpBehavior,
) -> Result<HttpResponse, AmenzbError> {
    http_get_follow_checked(url, accept, behavior, |_| Ok(()))
}

fn http_get_follow_checked<F>(
    url: &str,
    accept: &str,
    behavior: &NewznabHttpBehavior,
    mut validate_url: F,
) -> Result<HttpResponse, AmenzbError>
where
    F: FnMut(&str) -> Result<(), AmenzbError>,
{
    let mut current = url.to_string();
    validate_url(&current)?;
    for _ in 0..=MAX_REDIRECTS {
        let response = http_get_with_retry(&current, accept, behavior)?;
        if !matches!(response.status, 300..=399) {
            return Ok(response);
        }
        let Some(location) = header_value(&response, "location") else {
            return Ok(response);
        };
        current = resolve_location(&current, &location)?;
        validate_url(&current)?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(AmenzbError::Message(
        "ameNZB request exceeded redirect limit".to_string(),
    ))
}

fn http_get_with_retry(
    url: &str,
    accept: &str,
    behavior: &NewznabHttpBehavior,
) -> Result<HttpResponse, AmenzbError> {
    let mut total_wait = Duration::ZERO;
    let mut attempt = 1usize;
    loop {
        if !behavior.pre_request_delay.is_zero() {
            std::thread::sleep(behavior.pre_request_delay);
        }
        record_http_hit_budget_use(behavior)?;
        let response = http_get_once(url, accept, behavior)?;
        if response.status == 429 || matches!(response.status, 500 | 502 | 503 | 504) {
            let delay = retry_after_seconds(&response)
                .map(Duration::from_secs)
                .or_else(|| (response.status == 429).then_some(behavior.retry_default_delay))
                .unwrap_or(Duration::ZERO)
                .min(behavior.retry_max_delay);
            if !delay.is_zero()
                && attempt < behavior.retry_max_attempts.max(1)
                && total_wait + delay <= behavior.retry_total_budget
            {
                std::thread::sleep(delay);
                total_wait += delay;
                attempt += 1;
                continue;
            }
        }
        return Ok(response);
    }
}

fn http_get_once(
    url: &str,
    accept: &str,
    behavior: &NewznabHttpBehavior,
) -> Result<HttpResponse, AmenzbError> {
    let request = HttpRequest::new(url)
        .with_header("Accept", accept)
        .with_header("Accept-Encoding", "gzip")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("Cache-Control", "no-cache")
        .with_header("Pragma", "no-cache")
        .with_header("User-Agent", &behavior.user_agent);
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| AmenzbError::Message(format!("HTTP request failed: {error}")))?;
    let headers = response
        .headers()
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.to_string()))
        .collect::<HashMap<_, _>>();
    Ok(HttpResponse {
        status: response.status_code(),
        headers,
        body: String::from_utf8_lossy(&response.body()).to_string(),
    })
}

fn validate_download_url(config: &AmenzbConfig, url: &str) -> Result<(), AmenzbError> {
    let parsed = Url::parse(url)
        .map_err(|error| AmenzbError::Message(format!("invalid ameNZB subtitle URL: {error}")))?;
    let base = Url::parse(config.site_url())
        .map_err(|error| AmenzbError::Message(format!("invalid ameNZB base URL: {error}")))?;
    if parsed.scheme() != base.scheme() || parsed.host_str() != base.host_str() {
        return Err(AmenzbError::Message(
            "invalid ameNZB subtitle download host".to_string(),
        ));
    }
    let segments = parsed
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>());
    let valid_path = matches!(
        segments.as_deref(),
        Some(["release", release_id, "subtitles", subtitle_id])
            if release_id.chars().all(|ch| ch.is_ascii_digit())
                && subtitle_id.chars().all(|ch| ch.is_ascii_digit())
    );
    if !valid_path {
        return Err(AmenzbError::Message(
            "invalid ameNZB subtitle download path".to_string(),
        ));
    }
    Ok(())
}

fn retry_after_seconds(response: &HttpResponse) -> Option<u64> {
    header_value(response, "retry-after").and_then(|value| value.parse::<u64>().ok())
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
struct StoredHitBudget {
    hour_bucket: u64,
    hourly_count: u32,
    day_bucket: u64,
    daily_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HitBudgetSnapshot {
    hourly_count: u32,
    hourly_limit: u32,
    daily_count: u32,
    daily_limit: u32,
}

impl HitBudgetSnapshot {
    fn exhausted(self) -> bool {
        self.hourly_count >= self.hourly_limit || self.daily_count >= self.daily_limit
    }
}

fn record_http_hit_budget_use(behavior: &NewznabHttpBehavior) -> Result<(), AmenzbError> {
    let Some(budget) = &behavior.hit_budget else {
        return Ok(());
    };
    let state = load_hit_budget_state(budget)?;
    let (state, _) = advance_hit_budget_state(budget, state, current_epoch_seconds())?;
    save_hit_budget_state(budget, &state)
}

fn advance_hit_budget_state(
    budget: &NewznabHitBudget,
    mut state: StoredHitBudget,
    now_seconds: u64,
) -> Result<(StoredHitBudget, HitBudgetSnapshot), AmenzbError> {
    let hour_bucket = now_seconds / 3_600;
    let day_bucket = now_seconds / 86_400;
    if state.hour_bucket != hour_bucket {
        state.hour_bucket = hour_bucket;
        state.hourly_count = 0;
    }
    if state.day_bucket != day_bucket {
        state.day_bucket = day_bucket;
        state.daily_count = 0;
    }

    let snapshot = HitBudgetSnapshot {
        hourly_count: state.hourly_count,
        hourly_limit: budget.hourly_limit,
        daily_count: state.daily_count,
        daily_limit: budget.daily_limit,
    };
    if snapshot.exhausted() {
        return Err(AmenzbError::RateLimited(None));
    }

    state.hourly_count = state.hourly_count.saturating_add(1);
    state.daily_count = state.daily_count.saturating_add(1);
    Ok((
        state,
        HitBudgetSnapshot {
            hourly_count: state.hourly_count,
            hourly_limit: budget.hourly_limit,
            daily_count: state.daily_count,
            daily_limit: budget.daily_limit,
        },
    ))
}

fn load_hit_budget_state(budget: &NewznabHitBudget) -> Result<StoredHitBudget, AmenzbError> {
    let Some(raw) = var::get::<String>(&budget.var_key).map_err(|error| {
        AmenzbError::Message(format!("failed to read ameNZB hit budget: {error}"))
    })?
    else {
        return Ok(StoredHitBudget::default());
    };
    serde_json::from_str(&raw).map_err(|error| {
        AmenzbError::Message(format!("failed to parse ameNZB hit budget: {error}"))
    })
}

fn save_hit_budget_state(
    budget: &NewznabHitBudget,
    state: &StoredHitBudget,
) -> Result<(), AmenzbError> {
    let rendered = serde_json::to_string(state).map_err(|error| {
        AmenzbError::Message(format!("failed to encode ameNZB hit budget: {error}"))
    })?;
    var::set(&budget.var_key, rendered).map_err(|error| {
        AmenzbError::Message(format!("failed to store ameNZB hit budget: {error}"))
    })
}

fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn header_value(response: &HttpResponse, key: &str) -> Option<String> {
    response.headers.get(&key.to_ascii_lowercase()).cloned()
}

fn content_disposition_filename(response: &HttpResponse) -> Option<String> {
    let header = header_value(response, "content-disposition")?;
    for part in header.split(';').map(str::trim) {
        if let Some(value) = part.strip_prefix("filename*=") {
            return Some(decode_rfc5987_filename(value));
        }
        if let Some(value) = part.strip_prefix("filename=") {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

fn decode_rfc5987_filename(value: &str) -> String {
    let value = value.trim_matches('"');
    if let Some((_, encoded)) = value.split_once("''") {
        percent_decode(encoded)
    } else {
        percent_decode(value)
    }
}

fn resolve_location(current: &str, location: &str) -> Result<String, AmenzbError> {
    Url::parse(current)
        .and_then(|base| base.join(location))
        .map(|url| url.to_string())
        .map_err(|error| AmenzbError::Message(format!("invalid ameNZB redirect: {error}")))
}

fn subtitle_path_ids(href: &str) -> Option<(String, String)> {
    let segments = href
        .split('?')
        .next()
        .unwrap_or(href)
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        ["release", release_id, "subtitles", subtitle_id]
            if release_id.chars().all(|ch| ch.is_ascii_digit())
                && subtitle_id.chars().all(|ch| ch.is_ascii_digit()) =>
        {
            Some((release_id.to_string(), subtitle_id.to_string()))
        }
        _ => None,
    }
}

fn absolutize_url(site_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else {
        format!("{}{}", site_url.trim_end_matches('/'), href)
    }
}

fn table_cells(row: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cursor = 0;
    while let Some(td_offset) = row[cursor..].find("<td") {
        let tag_start = cursor + td_offset;
        let Some(content_start_offset) = row[tag_start..].find('>') else {
            break;
        };
        let content_start = tag_start + content_start_offset + 1;
        let Some(content_end_offset) = row[content_start..].find("</td>") else {
            break;
        };
        let content_end = content_start + content_end_offset;
        cells.push(row[content_start..content_end].to_string());
        cursor = content_end + "</td>".len();
    }
    cells
}

fn attr_value(html: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = html.find(&needle)? + needle.len();
    let end = html[start..].find('"')? + start;
    Some(html_unescape(&html[start..end]))
}

fn text_content(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    html_unescape(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

fn normalized_row_format(link: &SubtitleLink) -> Option<String> {
    match link.row_format.trim().to_ascii_lowercase().as_str() {
        "ass" | "ssa" | "srt" | "vtt" => Some(link.row_format.trim().to_ascii_lowercase()),
        _ => None,
    }
}

fn format_from_label(label: &str) -> Option<String> {
    let lowered = label.to_ascii_lowercase();
    ["ass", "ssa", "srt", "vtt"]
        .into_iter()
        .find(|format| lowered.contains(format))
        .map(ToString::to_string)
}

fn format_from_filename(filename: &str) -> Option<String> {
    let lowered = filename.to_ascii_lowercase();
    ["ass", "ssa", "srt", "vtt", "txt"]
        .into_iter()
        .find(|format| lowered.ends_with(&format!(".{format}")))
        .map(|format| {
            if format == "txt" {
                "txt".to_string()
            } else {
                format.to_string()
            }
        })
}

fn resolve_download_format(filename: Option<&str>, body: &str, fallback: &str) -> String {
    sniff_subtitle_format(body)
        .or_else(|| filename.and_then(format_from_filename))
        .unwrap_or_else(|| fallback.to_string())
}

fn sniff_subtitle_format(body: &str) -> Option<String> {
    let trimmed = body.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("WEBVTT") {
        Some("vtt".to_string())
    } else if trimmed.contains("[Script Info]") || trimmed.contains("[V4+ Styles]") {
        Some("ass".to_string())
    } else if trimmed.lines().take(5).any(|line| line.contains("-->")) {
        Some("srt".to_string())
    } else {
        None
    }
}

fn encode_query_pairs(pairs: Vec<(String, String)>) -> String {
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

fn split_metadata_list(value: &str) -> Vec<String> {
    value
        .split([',', '/', '|'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn normalize_attr_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn language_to_scryer(language: &str) -> &str {
    normalize_language(language)
}

fn amenzb_language(language: &str) -> &str {
    match language_to_scryer(language) {
        "eng" => "en",
        "jpn" => "ja",
        "deu" => "de",
        "fra" => "fr",
        "spa" => "es",
        "zho" => "zh",
        other => other,
    }
}

fn normalize_language(language: &str) -> &str {
    match language.trim().to_ascii_lowercase().as_str() {
        "ar" | "ara" | "arabic" => "ara",
        "de" | "deu" | "ger" | "german" => "deu",
        "en" | "en-us" | "en-gb" | "eng" | "english" => "eng",
        "es" | "es-419" | "es-es" | "spa" | "spanish" => "spa",
        "fr" | "fra" | "fre" | "french" => "fra",
        "it" | "ita" | "italian" => "ita",
        "ja" | "jp" | "jpn" | "japanese" => "jpn",
        "pt" | "pt-br" | "por" | "portuguese" => "por",
        "ru" | "rus" | "russian" => "rus",
        "zh" | "zho" | "chi" | "zh-hans" | "zh-hant" | "chinese" => "zho",
        _ => language,
    }
}

fn external_id(request: &SubtitlePluginSearchRequest, key: &str) -> Option<String> {
    request
        .external_ids
        .get(key)
        .and_then(|values| values.first())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn has_exact_provider_filter(request: &SubtitlePluginSearchRequest) -> bool {
    external_info_hash(request).is_some()
        || external_id(request, "anidb_id")
            .or_else(|| external_id(request, "anidb"))
            .is_some()
}

fn external_info_hash(request: &SubtitlePluginSearchRequest) -> Option<String> {
    external_id(request, "info_hash")
        .or_else(|| external_id(request, "info_hash_v1"))
        .or_else(|| external_id(request, "btih"))
        .map(|value| {
            value
                .trim()
                .trim_start_matches("urn:btih:")
                .to_ascii_lowercase()
        })
        .filter(|value| value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn single_requested_language(languages: &[String]) -> Option<String> {
    let mut languages = requested_languages(languages);
    languages.sort();
    languages.dedup();
    match languages.as_slice() {
        [language] => Some(language.clone()),
        _ => None,
    }
}

fn i32_to_u32(value: i32) -> Option<u32> {
    (value > 0).then_some(value as u32)
}

fn percent_decode(value: &str) -> String {
    url::form_urlencoded::parse(value.as_bytes())
        .map(|(key, value)| {
            if value.is_empty() {
                key.into_owned()
            } else {
                format!("{key}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<String>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value,
        value_source: ConfigFieldValueSource::User,
        role: None,
        host_binding: None,
        options: vec![],
        help_text: help_text.map(str::to_string),
    }
}

fn connection_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        role: Some(ConfigFieldRole::ConnectionUrl),
        ..field(
            key,
            label,
            ConfigFieldType::String,
            required,
            default_value.map(str::to_string),
            help_text,
        )
    }
}

fn plugin_error(error: AmenzbError) -> PluginError {
    match error {
        AmenzbError::RateLimited(retry_after_seconds) => PluginError {
            code: PluginErrorCode::RateLimited,
            public_message: "ameNZB rate limit reached".to_string(),
            debug_message: None,
            retry_after_seconds: retry_after_seconds.and_then(|value| i64::try_from(value).ok()),
        },
        AmenzbError::Message(message) => PluginError {
            code: PluginErrorCode::Permanent,
            public_message: message,
            debug_message: None,
            retry_after_seconds: None,
        },
    }
}

#[derive(Debug, Clone)]
enum AmenzbError {
    RateLimited(Option<u64>),
    Message(String),
}

impl AmenzbError {
    fn from_search_error(error: Error) -> Self {
        let message = error.to_string();
        if is_hit_budget_exhausted_error(&error)
            || message.contains("429")
            || message.to_ascii_lowercase().contains("rate limit")
        {
            Self::RateLimited(None)
        } else {
            Self::Message(message)
        }
    }
}

impl std::fmt::Display for AmenzbError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(_) => write!(formatter, "ameNZB rate limit reached"),
            Self::Message(message) => write!(formatter, "{message}"),
        }
    }
}

#[cfg(not(test))]
fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
fn config_value(_key: &str) -> Option<String> {
    None
}

#[cfg(not(test))]
fn config_bool(key: &str, default: bool) -> bool {
    config_value(key)
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Some(true),
            "0" | "false" | "no" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

#[cfg(test)]
fn config_bool(_key: &str, default: bool) -> bool {
    default
}

#[cfg(not(test))]
fn config_usize(key: &str, default: usize) -> usize {
    config_value(key)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
fn config_usize(_key: &str, default: usize) -> usize {
    default
}

#[cfg(not(test))]
fn config_u32(key: &str, default: u32) -> u32 {
    config_value(key)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
fn config_u32(_key: &str, default: u32) -> u32 {
    default
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn subtitle_request() -> SubtitlePluginSearchRequest {
        SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Episode,
            facet: Some("anime".to_string()),
            file_hash: None,
            imdb_id: None,
            series_imdb_id: None,
            title: "Kinomi Master".to_string(),
            title_aliases: vec![],
            title_candidates: vec![],
            year: None,
            season: Some(1),
            episode: Some(12),
            absolute_episode: None,
            external_ids: BTreeMap::new(),
            languages: vec!["eng".to_string()],
            release_group: Some("SubsPlease".to_string()),
            source: Some("WEB-DL".to_string()),
            video_codec: None,
            audio_codec: None,
            resolution: Some("1080p".to_string()),
            hearing_impaired: None,
            include_ai_translated: false,
            include_machine_translated: false,
        }
    }

    fn amenzb_config() -> AmenzbConfig {
        AmenzbConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: "key".to_string(),
            api_path: DEFAULT_API_PATH.to_string(),
            max_results: DEFAULT_MAX_RESULTS,
            max_detail_fetches: DEFAULT_MAX_DETAIL_FETCHES,
            category: Some(DEFAULT_CATEGORY.to_string()),
            healthy_only: false,
        }
    }

    #[test]
    fn descriptor_is_catalog_subtitle_provider_with_required_api_key() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Subtitle(subtitle) = descriptor.provider else {
            panic!("expected subtitle descriptor");
        };
        assert_eq!(descriptor.id, PROVIDER_ID);
        assert_eq!(subtitle.provider_type, PROVIDER_TYPE);
        assert_eq!(subtitle.default_base_url.as_deref(), Some(DEFAULT_BASE_URL));
        assert!(!subtitle.capabilities.supports_hash_lookup);
        assert_eq!(
            subtitle.capabilities.supported_media_kinds,
            vec![SubtitleQueryMediaKind::Episode]
        );
        let api_key = subtitle
            .config_fields
            .iter()
            .find(|field| field.key == "api_key")
            .expect("api key field");
        assert!(api_key.required);
    }

    #[test]
    fn provider_params_map_explicit_hash_ids_language_and_filters() {
        let mut request = subtitle_request();
        request.file_hash = Some("1234567890abcdef".to_string());
        request
            .external_ids
            .insert("anidb_id".to_string(), vec!["14821".to_string()]);
        request.external_ids.insert(
            "btih".to_string(),
            vec!["A1B2C3D4E5F6789012345678901234567890ABCD".to_string()],
        );
        let mut config = amenzb_config();
        config.healthy_only = true;

        let params = provider_params(&config, &request);

        assert!(params.contains("healthy=1"));
        assert!(params.contains("info_hash=a1b2c3d4e5f6789012345678901234567890abcd"));
        assert!(params.contains("anime_id=14821"));
        assert!(params.contains("season=1"));
        assert!(params.contains("ep=12"));
        assert!(params.contains("sub_lang=en"));
        assert!(params.contains("source=WEB-DL"));
        assert!(params.contains("resolution=1080p"));
        assert!(params.contains("release_group=SubsPlease"));
    }

    #[test]
    fn provider_params_do_not_map_media_file_hash_to_info_hash() {
        let mut request = subtitle_request();
        request.file_hash = Some("A1B2C3D4E5F6789012345678901234567890ABCD".to_string());

        let params = provider_params(&amenzb_config(), &request);

        assert!(!params.contains("info_hash="));
    }

    #[test]
    fn exact_anidb_search_clears_broad_title_query() {
        let mut request = subtitle_request();
        request.title_candidates = vec!["Noisy Parsed Title".to_string()];
        request
            .external_ids
            .insert("anidb".to_string(), vec!["14821".to_string()]);

        let search_request = search_request_for(&amenzb_config(), &request);
        let params = provider_params(&amenzb_config(), &request);

        assert!(search_request.query.is_empty());
        assert!(params.contains("anime_id=14821"));
        assert!(params.contains("season=1"));
        assert!(params.contains("ep=12"));
    }

    #[test]
    fn multi_language_request_omits_api_language_filter() {
        let mut request = subtitle_request();
        request.languages = vec!["eng".to_string(), "spa".to_string()];

        let params = provider_params(&amenzb_config(), &request);

        assert!(!params.contains("sub_lang="));
    }

    #[test]
    fn metadata_extractor_splits_subtitle_languages() {
        let pairs = vec![
            ("language".to_string(), "Japanese,English".to_string()),
            ("subs".to_string(), "English / Spanish".to_string()),
            ("guid".to_string(), "172993653".to_string()),
            ("grabs".to_string(), "1,234".to_string()),
        ];

        let (languages, grabs, extra) = amenzb_metadata_extractor(&pairs);

        assert_eq!(languages, vec!["Japanese", "English"]);
        assert_eq!(grabs, Some(1234));
        assert_eq!(extra["subtitle_languages"], json!(["English", "Spanish"]));
        assert_eq!(extra["guid"], json!("172993653"));
    }

    #[test]
    fn parses_subtitle_table_rows_and_related_release_hrefs() {
        let html = r#"
        <div id="subtitlesBody" class="collapse">
          <table><tbody>
            <tr>
              <td><code>eng</code></td>
              <td>English subs <span class="badge">Default</span></td>
              <td><code>other</code></td>
              <td>36 KB</td>
              <td><a href="/release/172993653/subtitles/10857">Download</a></td>
            </tr>
            <tr>
              <td><code>jpn</code></td>
              <td>SRT</td>
              <td><code>srt</code></td>
              <td>2 KB</td>
              <td><a href="/release/173016274/subtitles/41601">Download</a></td>
            </tr>
          </tbody></table>
        </div>
        "#;

        let links = parse_subtitle_links(html, DEFAULT_BASE_URL);

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].language, "eng");
        assert_eq!(links[0].release_id, "172993653");
        assert_eq!(links[0].subtitle_id, "10857");
        assert!(links[0].default_track);
        assert_eq!(
            links[1].url,
            "https://amenzb.moe/release/173016274/subtitles/41601"
        );
    }

    #[test]
    fn candidate_keeps_authoritative_download_ref() {
        let request = subtitle_request();
        let release = SearchResult {
            title: "[SubsPlease] Kinomi Master - 12".to_string(),
            grabs: Some(42),
            provider_extra: HashMap::from([("releasegroup".to_string(), json!("SubsPlease"))]),
            ..SearchResult::default()
        };
        let link = SubtitleLink {
            url: "https://amenzb.moe/release/172993653/subtitles/10857".to_string(),
            subtitle_id: "10857".to_string(),
            release_id: "172993653".to_string(),
            language: "eng".to_string(),
            label: "English subs".to_string(),
            row_format: "other".to_string(),
            size_label: Some("36 KB".to_string()),
            default_track: true,
        };

        let candidate = candidate_for_link(&request, &release, link).expect("candidate");
        let reference: AmenzbDownloadRef =
            serde_json::from_str(&candidate.provider_file_id).expect("download ref");

        assert_eq!(candidate.language, "eng");
        assert_eq!(candidate.uploader.as_deref(), Some("SubsPlease"));
        assert_eq!(reference.release_id, "172993653");
        assert_eq!(reference.subtitle_id, "10857");
        assert!(
            reference
                .url
                .ends_with("/release/172993653/subtitles/10857")
        );
    }

    #[test]
    fn validates_same_origin_subtitle_download_urls() {
        let config = amenzb_config();
        assert!(validate_download_url(&config, "https://amenzb.moe/release/1/subtitles/2").is_ok());
        assert!(
            validate_download_url(&config, "https://example.com/release/1/subtitles/2").is_err()
        );
        assert!(validate_download_url(&config, "https://amenzb.moe/download/1").is_err());
    }

    #[test]
    fn validates_redirect_targets_for_subtitle_downloads() {
        let config = amenzb_config();
        let same_origin = resolve_location(
            "https://amenzb.moe/release/1/subtitles/2",
            "/release/3/subtitles/4",
        )
        .expect("same-origin redirect");
        let cross_origin = resolve_location(
            "https://amenzb.moe/release/1/subtitles/2",
            "https://example.com/release/3/subtitles/4",
        )
        .expect("cross-origin redirect");

        assert!(validate_download_url(&config, &same_origin).is_ok());
        assert!(validate_download_url(&config, &cross_origin).is_err());
    }

    #[test]
    fn local_hit_budget_exhaustion_maps_to_rate_limited() {
        let budget = NewznabHitBudget {
            var_key: "amenzb_subtitles.test_hits".to_string(),
            hourly_limit: 1,
            daily_limit: 10,
        };
        let exhausted = StoredHitBudget {
            hour_bucket: 1,
            hourly_count: 1,
            day_bucket: 1,
            daily_count: 1,
        };

        assert!(matches!(
            advance_hit_budget_state(&budget, exhausted, 3_600),
            Err(AmenzbError::RateLimited(None))
        ));

        let prior_hour = StoredHitBudget {
            hour_bucket: 0,
            hourly_count: 1,
            day_bucket: 0,
            daily_count: 1,
        };
        let reset = advance_hit_budget_state(&budget, prior_hour, 3_600).expect("new hour resets");
        assert_eq!(reset.0.hourly_count, 1);
        assert_eq!(reset.0.daily_count, 2);
    }

    #[test]
    fn detects_format_from_filename_and_content() {
        assert_eq!(
            format_from_filename("episode.eng.ass").as_deref(),
            Some("ass")
        );
        assert_eq!(
            resolve_download_format(
                Some("[SubsPlease] Kinomi Master.eng.txt"),
                "[Script Info]\n[V4+ Styles]",
                "txt"
            ),
            "ass"
        );
        assert_eq!(
            sniff_subtitle_format("\u{feff}[Script Info]\n[V4+ Styles]").as_deref(),
            Some("ass")
        );
        assert_eq!(
            sniff_subtitle_format("1\n00:00:01,000 --> 00:00:02,000\nHello").as_deref(),
            Some("srt")
        );
        assert_eq!(
            sniff_subtitle_format("WEBVTT\n\n00:01 --> 00:02").as_deref(),
            Some("vtt")
        );
    }

    #[test]
    fn parses_content_disposition_filename() {
        let response = HttpResponse {
            status: 200,
            headers: HashMap::from([(
                "content-disposition".to_string(),
                "attachment; filename=\"episode.eng.txt\"".to_string(),
            )]),
            body: String::new(),
        };

        assert_eq!(
            content_disposition_filename(&response).as_deref(),
            Some("episode.eng.txt")
        );
    }
}
