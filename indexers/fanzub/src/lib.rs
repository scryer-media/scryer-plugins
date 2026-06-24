use extism_pdk::*;
use rss_indexer_common::*;

const PROVIDER_ID: &str = "fanzub";
const DEFAULT_BASE_URL: &str = "http://fanzub.com/rss/";
const DEFAULT_USER_AGENT: &str = "Scryer Fanzub Indexer/0.1";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
}

fn build_descriptor() -> PluginDescriptor {
    build_indexer_descriptor(DescriptorSpec {
        id: "fanzub",
        name: "Fanzub Indexer",
        version: env!("CARGO_PKG_VERSION"),
        provider_type: "fanzub",
        provider_aliases: vec![],
        source_kind: IndexerSourceKind::Usenet,
        protocols: vec![IndexerProtocol::Usenet],
        search: true,
        rss: true,
        supported_ids: anime_supported_ids(),
        supported_external_ids: anime_supported_external_ids(),
        supported_query_facets: vec!["anime".to_string()],
        feed_modes: vec![
            IndexerFeedMode::Recent,
            IndexerFeedMode::Rss,
            IndexerFeedMode::AutomaticSearch,
            IndexerFeedMode::InteractiveSearch,
        ],
        search_inputs: vec![
            IndexerSearchInput::TextQuery,
            IndexerSearchInput::Season,
            IndexerSearchInput::Episode,
            IndexerSearchInput::AbsoluteEpisode,
            IndexerSearchInput::Limit,
        ],
        config_fields: config_fields(),
        rate_limit_seconds: Some(2),
        page_size: Some(100),
        torrent: None,
    })
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let base_url = config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let anime_standard_format_search = config_bool("anime_standard_format_search");
    let url = fanzub_url(&base_url, &req, anime_standard_format_search);
    let http_config = RssHttpConfig::from_extism(DEFAULT_USER_AGENT);
    let mut options = RssParseOptions::usenet(PROVIDER_ID);
    options.use_enclosure_url = true;
    options.use_enclosure_length = true;
    options.page_size = 100;

    let response = execute_rss_urls(PROVIDER_ID, &[url], &http_config, &req, options)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    let mut fields = vec![
        connection_field(
            "base_url",
            "RSS URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("Fanzub RSS URL"),
        ),
        field(
            "anime_standard_format_search",
            "Anime Standard Format Search",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Include SxxExx and season-pack search variants"),
        ),
    ];
    fields.extend(http_config_fields(DEFAULT_USER_AGENT));
    fields
}

fn fanzub_url(base_url: &str, req: &SearchRequest, anime_standard_format_search: bool) -> String {
    let mut url = format!("{}?cat=anime&max=100", base_url.trim());
    let query = fanzub_query(req, anime_standard_format_search);
    if !query.is_empty() {
        url.push_str("&q=");
        url.push_str(&query);
    }
    url
}

fn fanzub_query(req: &SearchRequest, anime_standard_format_search: bool) -> String {
    let mut terms = Vec::new();
    for title in search_titles(req) {
        let title = clean_title(&title);
        if title.is_empty() {
            continue;
        }

        if let Some(absolute_episode) = req.absolute_episode.filter(|episode| *episode > 0) {
            terms.push(format!("\"{}%20{absolute_episode:02}\"", title));
            terms.push(format!("\"{}%20-%20{absolute_episode:02}\"", title));
        }

        if anime_standard_format_search {
            match (req.season, req.episode) {
                (Some(season), Some(episode)) if season > 0 && episode > 0 => {
                    terms.push(format!("\"{}%20S{season:02}E{episode:02}\"", title));
                    terms.push(format!("\"{}%20-%20S{season:02}E{episode:02}\"", title));
                }
                (Some(season), None) if season > 0 => {
                    terms.push(format!("\"{}%20S{season:02}\"", title));
                    terms.push(format!("\"{}%20-%20S{season:02}\"", title));
                }
                _ => {}
            }
        }

        if terms.is_empty() {
            terms.push(title.replace(' ', "%20"));
        }
    }

    dedupe(terms).join("|")
}

fn search_titles(req: &SearchRequest) -> Vec<String> {
    let mut titles = Vec::new();
    if !req.query.trim().is_empty() {
        titles.push(req.query.trim().to_string());
    }
    for alias in &req.tagged_aliases {
        if !alias.name.trim().is_empty() {
            titles.push(alias.name.trim().to_string());
        }
    }
    dedupe(titles)
}

fn clean_title(title: &str) -> String {
    title
        .chars()
        .filter(|ch| !matches!(ch, '!' | '?' | '`'))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_anime_text_capable_and_preserves_anime_ids() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        assert_eq!(
            indexer.capabilities.supported_query_facets,
            vec!["anime".to_string()]
        );
        assert_eq!(
            indexer.capabilities.supported_ids.get("anime"),
            Some(&vec!["tvdb_id".to_string(), "anidb_id".to_string()])
        );
        assert!(
            indexer
                .capabilities
                .search_inputs
                .contains(&IndexerSearchInput::TextQuery)
        );
    }
}
