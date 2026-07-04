use std::collections::HashMap;
use std::time::Duration;

use extism_pdk::*;
use newznab_common::{
    Capabilities, ConfigFieldDef, ConfigFieldType, IndexerCategoryModel, IndexerCategoryValueKind,
    IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol,
    IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind, NewznabConfig,
    NewznabHitBudget, NewznabHttpBehavior, PluginDescriptor, PluginResult, PluginSearchSubjectKind,
    ProviderDescriptor, SDK_VERSION, SearchRequest, SearchResponse, SearchResult,
    current_sdk_constraint, execute_full_search, extract_base_metadata, hit_budget_snapshot,
    is_hit_budget_exhausted_error, polite_http_get, standard_config_fields,
};
use url::Url;

const ANINZB_BASE_URL: &str = "https://aninzb.moe";
const MAX_PUBLIC_UI_PAGES: usize = 2;
const DEFAULT_HOURLY_HIT_CAP: u32 = 500;
const DEFAULT_DAILY_HIT_CAP: u32 = 3000;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
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
            source_kind: IndexerSourceKind::Usenet,
            capabilities: Capabilities {
                supported_ids: HashMap::from([
                    ("series".into(), vec!["tvdb_id".into(), "anidb_id".into()]),
                    ("anime".into(), vec!["tvdb_id".into(), "anidb_id".into()]),
                ]),
                deduplicates_aliases: false,
                season_param: Some("season".into()),
                episode_param: Some("ep".into()),
                query_param: Some("q".into()),
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
                    IndexerSearchInput::Category,
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
                    page_size: Some(100),
                    max_page_size: Some(100),
                    max_pages: Some(3),
                    api_quota_supported: true,
                    grab_quota_supported: false,
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: None,
                response_features: Some(IndexerResponseFeatures {
                    grabs: true,
                    comments: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: aninzb_config_fields(),
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    }
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let anidb_id = request_id(&req, "anidb_id");
    let has_tvdb_id = request_id(&req, "tvdb_id").is_some();
    let movie_shaped = request_is_movie_shaped(&req);
    if movie_shaped {
        return Ok(serde_json::to_string(&PluginResult::Ok(SearchResponse {
            results: vec![],
            api_current: None,
            api_max: None,
            grab_current: None,
            grab_max: None,
        }))?);
    }

    let mut config = NewznabConfig::from_extism()?;
    apply_aninzb_http_behavior(&mut config);
    config.page_size = config.page_size.min(100);

    let response = if let Some(anidb_id) = anidb_id.as_deref().filter(|_| !has_tvdb_id) {
        execute_anidb_archive_search(&config, &req, anidb_id)?
    } else {
        let api_response = execute_full_search(&config, &req, extract_base_metadata)?;
        if api_response.results.is_empty() && !hit_budget_exhausted_response(&api_response) {
            if let Some(anidb_id) = anidb_id.as_deref() {
                match execute_anidb_archive_search(&config, &req, anidb_id) {
                    Ok(archive_response) if !archive_response.results.is_empty() => {
                        archive_response
                    }
                    Ok(_) => api_response,
                    Err(err) if has_tvdb_id => {
                        log!(
                            LogLevel::Warn,
                            "aninzb anidb archive fallback failed after empty API response: {}",
                            err
                        );
                        api_response
                    }
                    Err(err) => return Err(err.into()),
                }
            } else {
                api_response
            }
        } else {
            api_response
        }
    };
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn apply_aninzb_http_behavior(config: &mut NewznabConfig) {
    config.http_behavior = NewznabHttpBehavior {
        plugin_id: "aninzb".to_string(),
        user_agent: USER_AGENT.to_string(),
        pre_request_delay: Duration::from_secs(3),
        retry_total_budget: Duration::from_secs(300),
        retry_default_delay: Duration::from_secs(60),
        retry_max_delay: Duration::from_secs(300),
        retry_max_attempts: 5,
        max_search_pages: 3,
        hit_budget: Some(NewznabHitBudget {
            var_key: "aninzb.http_hits".to_string(),
            hourly_limit: config_u32("hourly_hit_cap", DEFAULT_HOURLY_HIT_CAP),
            daily_limit: config_u32("daily_hit_cap", DEFAULT_DAILY_HIT_CAP),
        }),
    };
}

fn aninzb_config_fields() -> Vec<ConfigFieldDef> {
    let mut fields = standard_config_fields(Some(ANINZB_BASE_URL));
    fields.push(ConfigFieldDef {
        key: "hourly_hit_cap".to_string(),
        label: "Hourly Hit Cap".to_string(),
        field_type: ConfigFieldType::Number,
        required: false,
        default_value: Some(DEFAULT_HOURLY_HIT_CAP.to_string()),
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options: vec![],
        help_text: Some(
            "Maximum AniNZB HTTP requests per hour before searches return no results.".to_string(),
        ),
    });
    fields.push(ConfigFieldDef {
        key: "daily_hit_cap".to_string(),
        label: "Daily Hit Cap".to_string(),
        field_type: ConfigFieldType::Number,
        required: false,
        default_value: Some(DEFAULT_DAILY_HIT_CAP.to_string()),
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options: vec![],
        help_text: Some(
            "Maximum AniNZB HTTP requests per day before searches return no results.".to_string(),
        ),
    });
    fields
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

fn request_is_movie_shaped(req: &SearchRequest) -> bool {
    req.context
        .as_ref()
        .is_some_and(|context| context.subject_kind == PluginSearchSubjectKind::Movie)
        || req
            .facet
            .as_deref()
            .is_some_and(|facet| facet.trim().eq_ignore_ascii_case("movie"))
}

fn request_id(req: &SearchRequest, key: &str) -> Option<String> {
    req.ids
        .get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn execute_anidb_archive_search(
    config: &NewznabConfig,
    req: &SearchRequest,
    anidb_id: &str,
) -> Result<SearchResponse, Error> {
    let limit = archive_result_limit(config, req);
    let url = build_series_archive_url(&config.base_url, anidb_id)?;
    let body = match fetch_public_ui_page(&url, &config.http_behavior) {
        Ok(body) => body,
        Err(error) if is_hit_budget_exhausted_error(&error) => {
            return empty_response_with_hit_budget(config);
        }
        Err(error) => return Err(error),
    };
    let results = parse_series_archive_results(
        &body,
        &config.base_url,
        anidb_id,
        req.absolute_episode.or(req.episode),
        limit,
    );
    if results.is_empty() && req.absolute_episode.is_none() && req.episode.is_none() {
        return execute_anidb_public_ui_search(config, req, anidb_id);
    }

    response_with_hit_budget(config, results)
}

fn archive_result_limit(config: &NewznabConfig, req: &SearchRequest) -> usize {
    let max_results = config.page_size * config.http_behavior.max_search_pages.max(1);
    if req.limit == 0 {
        config.page_size.min(max_results).max(1)
    } else {
        req.limit.min(max_results).max(1)
    }
}

fn execute_anidb_public_ui_search(
    config: &NewznabConfig,
    req: &SearchRequest,
    anidb_id: &str,
) -> Result<SearchResponse, Error> {
    let page_size = config.page_size.min(100);
    let limit = req.limit.clamp(1, page_size * MAX_PUBLIC_UI_PAGES);
    let mut results = Vec::new();

    for page in 1..=MAX_PUBLIC_UI_PAGES {
        let url = build_public_ui_search_url(&config.base_url, anidb_id, page)?;
        let body = match fetch_public_ui_page(&url, &config.http_behavior) {
            Ok(body) => body,
            Err(error) if is_hit_budget_exhausted_error(&error) => {
                if results.is_empty() {
                    return empty_response_with_hit_budget(config);
                }
                break;
            }
            Err(error) => return Err(error),
        };
        let remaining = limit.saturating_sub(results.len());
        if remaining == 0 {
            break;
        }

        let mut page_results = parse_public_ui_results(&body, &config.base_url, remaining);
        page_results.retain(|result| {
            result
                .external_ids
                .get("anidb_id")
                .is_some_and(|value| value == anidb_id)
        });
        let page_count = page_results.len();
        results.extend(page_results);

        if results.len() >= limit || page_count == 0 || !public_ui_has_next_page(&body, page) {
            break;
        }
    }

    response_with_hit_budget(config, results)
}

fn hit_budget_exhausted_response(response: &SearchResponse) -> bool {
    matches!(
        (response.api_current, response.api_max),
        (Some(current), Some(max)) if current >= max
    )
}

fn empty_response_with_hit_budget(config: &NewznabConfig) -> Result<SearchResponse, Error> {
    response_with_hit_budget(config, vec![])
}

fn response_with_hit_budget(
    config: &NewznabConfig,
    results: Vec<SearchResult>,
) -> Result<SearchResponse, Error> {
    let (api_current, api_max) = hit_budget_snapshot(&config.http_behavior)?
        .map(|snapshot| snapshot.limiting_current_max())
        .unwrap_or((None, None));
    Ok(SearchResponse {
        results,
        api_current,
        api_max,
        grab_current: None,
        grab_max: None,
    })
}

fn build_public_ui_search_url(
    base_url: &str,
    anidb_id: &str,
    page: usize,
) -> Result<String, Error> {
    let mut url = Url::parse(base_url.trim())
        .map_err(|err| Error::msg(format!("invalid AniNZB base_url: {err}")))?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);

    {
        let mut query = url.query_pairs_mut();
        query.append_pair("q_series", anidb_id);
        if page > 1 {
            query.append_pair("page", &page.to_string());
        }
    }

    Ok(url.to_string())
}

fn build_series_archive_url(base_url: &str, anidb_id: &str) -> Result<String, Error> {
    let anidb_id = anidb_id.trim();
    if anidb_id.is_empty() || !anidb_id.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(Error::msg("invalid AniDB id for AniNZB archive lookup"));
    }

    let mut url = Url::parse(base_url.trim())
        .map_err(|err| Error::msg(format!("invalid AniNZB base_url: {err}")))?;
    url.set_path(&format!("/series/{anidb_id}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn fetch_public_ui_page(url: &str, behavior: &NewznabHttpBehavior) -> Result<String, Error> {
    let (status, body) = polite_http_get(
        url,
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        behavior,
    )?;
    if status >= 400 {
        return Err(Error::msg(format!(
            "AniNZB public UI returned HTTP {status}"
        )));
    }

    Ok(body)
}

fn parse_series_archive_results(
    body: &str,
    base_url: &str,
    anidb_id: &str,
    episode: Option<u32>,
    limit: usize,
) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = body;

    while let Some(h2_start) = rest.find("<h2") {
        rest = &rest[h2_start..];
        let Some(h2_open_end) = rest.find('>') else {
            break;
        };
        let Some(h2_close) = rest.find("</h2>") else {
            break;
        };
        let episode_label = normalize_html_text(&strip_tags(&rest[h2_open_end + 1..h2_close]));
        let section_after_heading = &rest[h2_close + "</h2>".len()..];
        let next_h2 = section_after_heading
            .find("<h2")
            .unwrap_or(section_after_heading.len());
        let section = &section_after_heading[..next_h2];
        rest = &section_after_heading[next_h2..];

        let Some(archive_episode) = archive_episode_from_label(&episode_label) else {
            continue;
        };
        if episode.is_some_and(|expected| expected != archive_episode) {
            continue;
        }

        let remaining = limit.saturating_sub(results.len());
        if remaining == 0 {
            break;
        }
        let mut section_results =
            parse_series_archive_section(section, base_url, anidb_id, archive_episode, remaining);
        results.append(&mut section_results);
        if results.len() >= limit {
            break;
        }
    }

    results
}

fn parse_series_archive_section(
    section: &str,
    base_url: &str,
    anidb_id: &str,
    episode: u32,
    limit: usize,
) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = section;

    while let Some(start) = rest.find("<tr") {
        rest = &rest[start..];
        let Some(end) = rest.find("</tr>") else {
            break;
        };
        let row = &rest[..end + "</tr>".len()];
        rest = &rest[end + "</tr>".len()..];

        if let Some(result) = parse_series_archive_row(row, base_url, anidb_id, episode) {
            results.push(result);
            if results.len() >= limit {
                break;
            }
        }
    }

    results
}

fn parse_series_archive_row(
    row: &str,
    base_url: &str,
    anidb_id: &str,
    episode: u32,
) -> Option<SearchResult> {
    let cells = table_cells(row);
    if cells.len() < 6 || cells[0].eq_ignore_ascii_case("source") {
        return None;
    }

    let source = cells[0].trim();
    let title = cells[1].trim().to_string();
    if title.is_empty() {
        return None;
    }
    let group = empty_dash_as_none(cells[2].trim());
    let size_bytes = parse_archive_size(cells[3].trim());
    let published_at = empty_dash_as_none(cells[4].trim()).map(ToOwned::to_owned);
    let download_href = find_href_containing(row, "/tosho/nzb/")
        .or_else(|| find_href_containing(row, "/api/nzb/"))?;
    let download_url = join_url(base_url, &download_href);
    let archive_id = archive_download_id(&download_href);

    let mut external_ids = HashMap::new();
    external_ids.insert("anidb_id".to_string(), anidb_id.to_string());

    let mut provider_extra = HashMap::new();
    provider_extra.insert(
        "search_surface".to_string(),
        serde_json::Value::from("series_archive"),
    );
    provider_extra.insert("source".to_string(), serde_json::Value::from(source));
    provider_extra.insert("episode".to_string(), serde_json::Value::from(episode));
    if let Some(group) = group {
        provider_extra.insert("group".to_string(), serde_json::Value::from(group));
    }
    if let Some(archive_id) = archive_id.as_deref() {
        provider_extra.insert(
            "archive_item_id".to_string(),
            serde_json::Value::from(archive_id),
        );
    }

    Some(SearchResult {
        title,
        link: download_url.clone(),
        download_url,
        size_bytes,
        published_at,
        provider_extra,
        guid: Some(
            archive_id
                .map(|id| format!("aninzb:{}:{}", source.to_ascii_lowercase(), id))
                .unwrap_or_else(|| format!("aninzb:archive:{anidb_id}:{episode}")),
        ),
        info_url: join_url(base_url, &format!("/series/{anidb_id}")),
        source_kind: Some(IndexerSourceKind::Usenet),
        protocol: Some(IndexerProtocol::Usenet),
        external_ids,
        categories: vec!["5070".to_string()],
        provider_categories: vec!["TV/Anime".to_string()],
        ..SearchResult::default()
    })
}

fn parse_public_ui_results(body: &str, base_url: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = body;

    while let Some(start) = rest.find("<tr") {
        rest = &rest[start..];
        let Some(end) = rest.find("</tr>") else {
            break;
        };
        let row = &rest[..end + "</tr>".len()];
        rest = &rest[end + "</tr>".len()..];

        if let Some(result) = parse_public_ui_row(row, base_url) {
            results.push(result);
            if results.len() >= limit {
                break;
            }
        }
    }

    results
}

fn parse_public_ui_row(row: &str, base_url: &str) -> Option<SearchResult> {
    let release_anchor = find_anchor_with_class(row, "release-name-link")?;
    let title = anchor_text(release_anchor)?;
    let release_href = extract_attr(release_anchor, "href")?;
    let download_href = find_href_containing(row, "/api/nzb/")?;

    let info_url = join_url(base_url, &release_href);
    let download_url = join_url(base_url, &download_href);
    let published_at = extract_attr(row, "data-utc");
    let mut external_ids = HashMap::new();
    if let Some(anidb_id) = digits_after(row, "https://anidb.net/anime/") {
        external_ids.insert("anidb_id".to_string(), anidb_id);
    }
    if let Some(tvdb_id) = digits_after(row, "https://thetvdb.com/dereferrer/series/") {
        external_ids.insert("tvdb_id".to_string(), tvdb_id);
    }

    let mut provider_extra = HashMap::new();
    if let Some(summary) = release_summary(row) {
        provider_extra.insert(
            "anime_summary".to_string(),
            serde_json::Value::from(summary),
        );
    }
    provider_extra.insert(
        "search_surface".to_string(),
        serde_json::Value::from("public_ui"),
    );

    Some(SearchResult {
        title,
        link: info_url.clone(),
        download_url,
        published_at,
        provider_extra,
        guid: info_url.clone(),
        info_url,
        source_kind: Some(IndexerSourceKind::Usenet),
        protocol: Some(IndexerProtocol::Usenet),
        external_ids,
        categories: vec!["5070".to_string()],
        provider_categories: vec!["TV/Anime".to_string()],
        ..SearchResult::default()
    })
}

fn table_cells(row: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut rest = row;
    while let Some(start) = rest.find("<td") {
        rest = &rest[start..];
        let Some(open_end) = rest.find('>') else {
            break;
        };
        let Some(close) = rest.find("</td>") else {
            break;
        };
        cells.push(normalize_html_text(&strip_tags(&rest[open_end + 1..close])));
        rest = &rest[close + "</td>".len()..];
    }
    cells
}

fn archive_episode_from_label(label: &str) -> Option<u32> {
    let remainder = label.trim().strip_prefix("Ep ")?;
    remainder
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse::<u32>()
        .ok()
}

fn empty_dash_as_none(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && value != "—" && value != "-").then_some(value)
}

fn parse_archive_size(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let amount = parts.next()?.replace(',', "").parse::<f64>().ok()?;
    let unit = parts.next()?.trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "b" | "byte" | "bytes" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((amount * multiplier).round() as i64)
}

fn archive_download_id(href: &str) -> Option<String> {
    href.rsplit('/')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn find_anchor_with_class<'a>(fragment: &'a str, class_name: &str) -> Option<&'a str> {
    let class_marker = format!("class=\"{class_name}\"");
    let class_idx = fragment.find(&class_marker)?;
    let anchor_start = fragment[..class_idx].rfind("<a")?;
    let anchor_end = fragment[class_idx..].find("</a>")? + class_idx + "</a>".len();
    Some(&fragment[anchor_start..anchor_end])
}

fn anchor_text(anchor: &str) -> Option<String> {
    let start = anchor.find('>')? + 1;
    let end = anchor[start..].find("</a>")? + start;
    Some(normalize_html_text(&anchor[start..end]))
}

fn release_summary(row: &str) -> Option<String> {
    let marker = "class=\"release-anime-sub\"";
    let marker_idx = row.find(marker)?;
    let content_start = row[marker_idx..].find('>')? + marker_idx + 1;
    let content_end = row[content_start..]
        .find("<span")
        .map(|idx| content_start + idx)
        .unwrap_or_else(|| {
            row[content_start..]
                .find("</div>")
                .map_or(row.len(), |idx| content_start + idx)
        });
    let text = strip_tags(&row[content_start..content_end]);
    let text = normalize_html_text(&text);
    (!text.is_empty()).then_some(text)
}

fn find_href_containing(fragment: &str, needle: &str) -> Option<String> {
    let mut rest = fragment;
    while let Some(idx) = rest.find("href=\"") {
        rest = &rest[idx + "href=\"".len()..];
        let end = rest.find('"')?;
        let href = html_decode(&rest[..end]);
        if href.contains(needle) {
            return Some(href);
        }
        rest = &rest[end + 1..];
    }
    None
}

fn extract_attr(fragment: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    let start = fragment.find(&pattern)? + pattern.len();
    let end = fragment[start..].find('"')? + start;
    Some(html_decode(&fragment[start..end]))
}

fn digits_after(fragment: &str, marker: &str) -> Option<String> {
    let start = fragment.find(marker)? + marker.len();
    let digits: String = fragment[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    (!digits.is_empty()).then_some(digits)
}

fn join_url(base_url: &str, href: &str) -> Option<String> {
    Url::parse(base_url.trim())
        .ok()
        .and_then(|base| base.join(href).ok())
        .map(|url| url.to_string())
}

fn strip_tags(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_tag = false;
    for ch in value.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
}

fn normalize_html_text(value: &str) -> String {
    html_decode(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_decode(value: &str) -> String {
    html_escape::decode_html_entities(value).to_string()
}

fn public_ui_has_next_page(body: &str, page: usize) -> bool {
    let next_page = page + 1;
    body.contains(&format!("page={next_page}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_matches_aninzb_current_caps() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        let base_url = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "base_url")
            .expect("base_url field");
        let api_key = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "api_key")
            .expect("api_key field");
        let hourly_hit_cap = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "hourly_hit_cap")
            .expect("hourly_hit_cap field");
        let daily_hit_cap = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "daily_hit_cap")
            .expect("daily_hit_cap field");

        assert_eq!(base_url.default_value.as_deref(), Some(ANINZB_BASE_URL));
        assert!(!api_key.required);
        assert_eq!(hourly_hit_cap.default_value.as_deref(), Some("500"));
        assert_eq!(daily_hit_cap.default_value.as_deref(), Some("3000"));
        assert_eq!(
            indexer.capabilities.supported_ids.get("anime"),
            Some(&vec!["tvdb_id".to_string(), "anidb_id".to_string()])
        );
        assert!(indexer.capabilities.tvdb_search);
        assert!(indexer.capabilities.anidb_search);
        assert!(!indexer.capabilities.imdb_search);
        let limits = indexer.capabilities.limits.as_ref().expect("limits");
        assert_eq!(limits.page_size, Some(100));
        assert_eq!(limits.max_page_size, Some(100));
        assert_eq!(limits.max_pages, Some(3));
        assert!(limits.api_quota_supported);
    }

    #[test]
    fn aninzb_http_behavior_is_cautious_and_budgeted() {
        let mut config = NewznabConfig {
            base_url: ANINZB_BASE_URL.to_string(),
            api_key: String::new(),
            api_path: "/api".to_string(),
            additional_params: String::new(),
            page_size: 100,
            http_behavior: NewznabHttpBehavior::default(),
        };

        apply_aninzb_http_behavior(&mut config);

        assert_eq!(config.http_behavior.plugin_id, "aninzb");
        assert_eq!(config.http_behavior.user_agent, USER_AGENT);
        assert_eq!(
            config.http_behavior.pre_request_delay,
            Duration::from_secs(3)
        );
        assert_eq!(
            config.http_behavior.retry_total_budget,
            Duration::from_secs(300)
        );
        assert_eq!(
            config.http_behavior.retry_default_delay,
            Duration::from_secs(60)
        );
        assert_eq!(config.http_behavior.retry_max_attempts, 5);
        assert_eq!(config.http_behavior.max_search_pages, 3);
        let budget = config
            .http_behavior
            .hit_budget
            .as_ref()
            .expect("hit budget");
        assert_eq!(budget.hourly_limit, DEFAULT_HOURLY_HIT_CAP);
        assert_eq!(budget.daily_limit, DEFAULT_DAILY_HIT_CAP);
    }

    #[test]
    fn series_archive_url_uses_anidb_id_path() {
        assert_eq!(
            build_series_archive_url(ANINZB_BASE_URL, "17617").unwrap(),
            "https://aninzb.moe/series/17617"
        );
        assert!(build_series_archive_url(ANINZB_BASE_URL, "frieren").is_err());
    }

    #[test]
    fn series_archive_parser_extracts_tosho_rows_for_requested_episode() {
        let html = r#"
<h2>Ep 1 <span class="muted-line">(2)</span></h2>
<table><tbody>
  <tr>
    <td><span class="source-tag source-tag--tosho">TOSHO</span></td>
    <td>[9volt] Sousou no Frieren - 01 [BBE15D15].mkv</td>
    <td>—</td>
    <td>3.23 GB</td>
    <td>2023-10-04</td>
    <td><a href="/tosho/nzb/580020">Download</a></td>
  </tr>
</tbody></table>
<h2>Ep 2 <span class="muted-line">(1)</span></h2>
<table><tbody>
  <tr>
    <td><span class="source-tag source-tag--tosho">TOSHO</span></td>
    <td>[SubsPlease] Sousou no Frieren - 02 (1080p).mkv</td>
    <td>SubsPlease</td>
    <td>1.01 GB</td>
    <td>2023-10-06</td>
    <td><a href="/tosho/nzb/580021">Download</a></td>
  </tr>
</tbody></table>
"#;

        let results = parse_series_archive_results(html, ANINZB_BASE_URL, "17617", Some(2), 10);
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(
            result.title,
            "[SubsPlease] Sousou no Frieren - 02 (1080p).mkv"
        );
        assert_eq!(
            result.download_url.as_deref(),
            Some("https://aninzb.moe/tosho/nzb/580021")
        );
        assert_eq!(result.guid.as_deref(), Some("aninzb:tosho:580021"));
        assert_eq!(result.size_bytes, Some(1_084_479_242));
        assert_eq!(result.published_at.as_deref(), Some("2023-10-06"));
        assert_eq!(
            result
                .provider_extra
                .get("search_surface")
                .and_then(|v| v.as_str()),
            Some("series_archive")
        );
        assert_eq!(
            result.provider_extra.get("group").and_then(|v| v.as_str()),
            Some("SubsPlease")
        );
        assert_eq!(
            result.external_ids.get("anidb_id").map(String::as_str),
            Some("17617")
        );
    }

    #[test]
    fn public_ui_url_searches_by_anidb_series_id() {
        assert_eq!(
            build_public_ui_search_url(ANINZB_BASE_URL, "19381", 1).unwrap(),
            "https://aninzb.moe/?q_series=19381"
        );
        assert_eq!(
            build_public_ui_search_url(ANINZB_BASE_URL, "19381", 2).unwrap(),
            "https://aninzb.moe/?q_series=19381&page=2"
        );
    }

    #[test]
    fn public_ui_parser_extracts_current_release_metadata() {
        let html = r#"
<table><tbody>
  <tr>
    <td class="col-added mono"><div data-utc="2026-06-03T19:07:57Z"></div></td>
    <td class="col-name">
      <a class="release-name-link" href="/2797">NIPPON &amp; FRIENDS S01E08</a>
      <div class="release-anime-sub">
        Nippon &amp; Friends Season 1
        <span class="release-anime-links">
          <a href="https://anidb.net/anime/19861">AniDB &uarr;</a>
          <a href="https://thetvdb.com/dereferrer/series/473228">TVDB &uarr;</a>
        </span>
      </div>
    </td>
    <td class="col-nzb">
      <a class="btn btn-ghost btn-sm" href="/api/nzb/2797/NIPPON%20FRIENDS.nzb">Download</a>
    </td>
  </tr>
</tbody></table>
"#;

        let results = parse_public_ui_results(html, ANINZB_BASE_URL, 10);
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.title, "NIPPON & FRIENDS S01E08");
        assert_eq!(
            result.download_url.as_deref(),
            Some("https://aninzb.moe/api/nzb/2797/NIPPON%20FRIENDS.nzb")
        );
        assert_eq!(result.info_url.as_deref(), Some("https://aninzb.moe/2797"));
        assert_eq!(
            result.external_ids.get("anidb_id").map(String::as_str),
            Some("19861")
        );
        assert_eq!(
            result.external_ids.get("tvdb_id").map(String::as_str),
            Some("473228")
        );
        assert_eq!(result.source_kind, Some(IndexerSourceKind::Usenet));
        assert_eq!(result.protocol, Some(IndexerProtocol::Usenet));
        assert_eq!(result.categories, vec!["5070".to_string()]);
        assert_eq!(
            result
                .provider_extra
                .get("search_surface")
                .and_then(|value| value.as_str()),
            Some("public_ui")
        );
    }

    #[test]
    fn movie_requests_are_recognized_as_unsupported_shape() {
        let request = SearchRequest {
            facet: Some("movie".to_string()),
            query: "12 Years a Slave".to_string(),
            ..SearchRequest::default()
        };

        assert!(request_is_movie_shaped(&request));
    }

    #[test]
    fn request_id_trims_and_ignores_empty_values() {
        let request = SearchRequest {
            ids: HashMap::from([
                ("anidb_id".to_string(), " 19381 ".to_string()),
                ("tvdb_id".to_string(), " ".to_string()),
            ]),
            ..SearchRequest::default()
        };

        assert_eq!(request_id(&request, "anidb_id").as_deref(), Some("19381"));
        assert_eq!(request_id(&request, "tvdb_id"), None);
    }
}
