use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    Capabilities, IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor,
    IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures,
    IndexerSearchInput, IndexerSourceKind, NewznabConfig, PluginDescriptor, PluginResult,
    PluginSearchSubjectKind, ProviderDescriptor, SDK_VERSION, SearchRequest, SearchResponse,
    SearchResult, current_sdk_constraint, execute_full_search, extract_base_metadata,
    standard_config_fields,
};
use url::Url;

const ANINZB_BASE_URL: &str = "https://aninzb.moe";
const MAX_PUBLIC_UI_PAGES: usize = 5;
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
                supported_query_facets: vec!["anime".into()],
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
                    max_pages: Some(10),
                    api_quota_supported: false,
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
            config_fields: standard_config_fields(Some(ANINZB_BASE_URL)),
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    }
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let mut config = NewznabConfig::from_extism()?;
    config.page_size = config.page_size.min(100);
    let anidb_id = request_id(&req, "anidb_id");
    let has_tvdb_id = request_id(&req, "tvdb_id").is_some();
    let movie_shaped = request_is_movie_shaped(&req);

    let response = if let Some(anidb_id) = anidb_id
        .as_deref()
        .filter(|_| !has_tvdb_id && !movie_shaped)
    {
        execute_anidb_public_ui_search(&config, &req, anidb_id)?
    } else {
        let api_response = execute_full_search(&config, &req, extract_base_metadata)?;
        if api_response.results.is_empty() && !movie_shaped {
            if let Some(anidb_id) = anidb_id.as_deref() {
                match execute_anidb_public_ui_search(&config, &req, anidb_id) {
                    Ok(ui_response) if !ui_response.results.is_empty() => ui_response,
                    Ok(_) => api_response,
                    Err(err) if has_tvdb_id => {
                        log!(
                            LogLevel::Warn,
                            "aninzb anidb public UI fallback failed after empty API response: {}",
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
        let body = fetch_public_ui_page(&url)?;
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

    Ok(SearchResponse {
        results,
        api_current: None,
        api_max: None,
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

fn fetch_public_ui_page(url: &str) -> Result<String, Error> {
    let request = HttpRequest::new(url)
        .with_header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("User-Agent", USER_AGENT);

    log!(
        LogLevel::Debug,
        "http_trace plugin=aninzb_public_ui method=GET url={}",
        url
    );

    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|err| Error::msg(format!("AniNZB public UI request failed: {err}")))?;
    let status = response.status_code();
    if status >= 400 {
        return Err(Error::msg(format!(
            "AniNZB public UI returned HTTP {status}"
        )));
    }

    Ok(String::from_utf8_lossy(&response.body()).to_string())
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

        assert_eq!(base_url.default_value.as_deref(), Some(ANINZB_BASE_URL));
        assert!(!api_key.required);
        assert_eq!(
            indexer.capabilities.supported_ids.get("anime"),
            Some(&vec!["tvdb_id".to_string(), "anidb_id".to_string()])
        );
        assert!(indexer.capabilities.tvdb_search);
        assert!(indexer.capabilities.anidb_search);
        assert!(!indexer.capabilities.imdb_search);
        assert_eq!(
            indexer
                .capabilities
                .limits
                .as_ref()
                .and_then(|limits| limits.max_page_size),
            Some(100)
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
