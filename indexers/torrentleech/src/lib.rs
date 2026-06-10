use extism_pdk::*;
use rss_indexer_common::*;

const PROVIDER_ID: &str = "torrentleech";
const DEFAULT_BASE_URL: &str = "http://rss.torrentleech.org";
const DEFAULT_USER_AGENT: &str = "Scryer TorrentLeech Indexer/0.1";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_indexer_descriptor(DescriptorSpec {
        id: "torrentleech",
        name: "TorrentLeech Indexer",
        version: env!("CARGO_PKG_VERSION"),
        provider_type: "torrentleech",
        provider_aliases: vec!["torrent-leech".to_string()],
        source_kind: IndexerSourceKind::Torrent,
        protocols: vec![IndexerProtocol::Torrent],
        search: false,
        rss: true,
        feed_modes: vec![IndexerFeedMode::Recent, IndexerFeedMode::Rss],
        search_inputs: vec![IndexerSearchInput::Limit],
        config_fields: config_fields(),
        rate_limit_seconds: Some(2),
        page_size: Some(200),
        torrent: Some(IndexerTorrentCapabilities {
            reports_seeders: true,
            reports_peers: true,
            reports_info_hash: true,
            reports_magnet_uri: true,
            supports_private_tracker_flags: true,
            supports_seed_requirements: true,
            ..IndexerTorrentCapabilities::default()
        }),
    });

    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let base_url = config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let api_key = required_config("api_key")?;
    let feed_url = format!("{}/{}", base_url.trim().trim_end_matches('/'), api_key);
    let http_config = RssHttpConfig::from_extism(DEFAULT_USER_AGENT);
    let mut options = RssParseOptions::torrent(PROVIDER_ID);
    options.use_guid_info_url = true;
    options.parse_seeders_in_description = true;

    let response = execute_rss_urls(PROVIDER_ID, &[feed_url], &http_config, &req, options)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    let mut fields = vec![
        connection_field(
            "base_url",
            "Website URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("TorrentLeech RSS URL"),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("TorrentLeech RSS key"),
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
