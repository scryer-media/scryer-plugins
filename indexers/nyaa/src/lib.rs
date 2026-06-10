use extism_pdk::*;
use rss_indexer_common::*;

const PROVIDER_ID: &str = "nyaa";
const DEFAULT_ADDITIONAL_PARAMS: &str = "&cats=1_0&filter=1";
const DEFAULT_USER_AGENT: &str = "Scryer Nyaa Indexer/0.1";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_indexer_descriptor(DescriptorSpec {
        id: "nyaa",
        name: "Nyaa Indexer",
        version: env!("CARGO_PKG_VERSION"),
        provider_type: "nyaa",
        provider_aliases: vec![],
        source_kind: IndexerSourceKind::Torrent,
        protocols: vec![IndexerProtocol::Torrent],
        search: true,
        rss: true,
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
        page_size: Some(200),
        torrent: Some(IndexerTorrentCapabilities {
            reports_seeders: true,
            reports_peers: true,
            reports_info_hash: true,
            reports_magnet_uri: true,
            supports_private_tracker_flags: false,
            ..IndexerTorrentCapabilities::default()
        }),
    });

    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let base_url = required_config("base_url")?;
    let additional_params =
        config_value("additional_params").unwrap_or_else(|| DEFAULT_ADDITIONAL_PARAMS.to_string());
    let anime_standard_format_search = config_bool("anime_standard_format_search");
    let urls = nyaa_urls(
        &base_url,
        &additional_params,
        &req,
        anime_standard_format_search,
    );
    let http_config = RssHttpConfig::from_extism(DEFAULT_USER_AGENT);
    let mut options = RssParseOptions::torrent(PROVIDER_ID);
    options.use_guid_info_url = true;
    options.size_element_name = Some("size");
    options.info_hash_element_name = Some("infoHash");
    options.peers_element_name = Some("leechers");
    options.leechers_element_name = Some("leechers");
    options.seeds_element_name = Some("seeders");
    options.calculate_peers_as_sum = true;

    let response = execute_rss_urls(PROVIDER_ID, &urls, &http_config, &req, options)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    let mut fields = vec![
        connection_field(
            "base_url",
            "Website URL",
            true,
            None,
            Some("Nyaa website URL, for example https://nyaa.si"),
        ),
        field(
            "anime_standard_format_search",
            "Anime Standard Format Search",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Include SxxExx and season-pack search variants"),
        ),
        field(
            "additional_params",
            "Additional Parameters",
            ConfigFieldType::String,
            false,
            Some(DEFAULT_ADDITIONAL_PARAMS),
            Some("Extra query parameters appended to the Nyaa RSS request"),
        ),
        field(
            "minimum_seeders",
            "Minimum Seeders",
            ConfigFieldType::Number,
            false,
            Some("1"),
            Some("Minimum seeders preference for host-side release decisions"),
        ),
    ];
    fields.extend(http_config_fields(DEFAULT_USER_AGENT));
    fields
}

fn nyaa_urls(
    base_url: &str,
    additional_params: &str,
    req: &SearchRequest,
    anime_standard_format_search: bool,
) -> Vec<String> {
    let base = format!(
        "{}/?page=rss{}",
        base_url.trim().trim_end_matches('/'),
        additional_params
    );
    let terms = nyaa_terms(req, anime_standard_format_search);
    if terms.is_empty() {
        return vec![base];
    }

    terms
        .into_iter()
        .map(|term| format!("{base}&term={term}"))
        .collect()
}

fn nyaa_terms(req: &SearchRequest, anime_standard_format_search: bool) -> Vec<String> {
    let mut terms = Vec::new();
    for title in search_titles(req) {
        let prepared = prepare_query(&title);
        if prepared.is_empty() {
            continue;
        }

        if let Some(absolute_episode) = req.absolute_episode.filter(|episode| *episode > 0) {
            terms.push(format!("{prepared}+{absolute_episode}"));
            if absolute_episode < 10 {
                terms.push(format!("{prepared}+{absolute_episode:02}"));
            }
        }

        if anime_standard_format_search {
            match (req.season, req.episode) {
                (Some(season), Some(episode)) if season > 0 && episode > 0 => {
                    terms.push(format!("{prepared}+s{season:02}e{episode:02}"));
                }
                (Some(season), None) if season > 0 => {
                    terms.push(format!("{prepared}+s{season:02}"));
                }
                _ => {}
            }
        }

        if terms.is_empty() {
            terms.push(prepared);
        }
    }

    dedupe(terms)
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

fn prepare_query(query: &str) -> String {
    query.trim().replace(' ', "+")
}
