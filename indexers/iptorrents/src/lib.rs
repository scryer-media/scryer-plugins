use extism_pdk::*;
use rss_indexer_common::*;

const PROVIDER_ID: &str = "iptorrents";
const DEFAULT_USER_AGENT: &str = "Scryer IPTorrents Indexer/0.1";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_indexer_descriptor(DescriptorSpec {
        id: "iptorrents",
        name: "IPTorrents Indexer",
        version: env!("CARGO_PKG_VERSION"),
        provider_type: "iptorrents",
        provider_aliases: vec!["ip-torrents".to_string()],
        source_kind: IndexerSourceKind::Torrent,
        protocols: vec![IndexerProtocol::Torrent],
        search: false,
        rss: true,
        query_only: false,
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
    let feed_url = required_config("feed_url")?;
    if !is_direct_download_feed_url(&feed_url) {
        return Err(Error::msg(
            "IPTorrents feed_url must be the direct-download RSS URL containing ;download",
        )
        .into());
    }
    let http_config = RssHttpConfig::from_extism(DEFAULT_USER_AGENT);
    let mut options = RssParseOptions::torrent(PROVIDER_ID);
    options.parse_size_in_description = true;

    let response = execute_rss_urls(PROVIDER_ID, &[feed_url], &http_config, &req, options)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    let mut fields = vec![
        connection_field(
            "feed_url",
            "Feed URL",
            true,
            None,
            Some("IPTorrents direct-download RSS URL ending with ;download"),
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

fn is_direct_download_feed_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    let Some((_, query)) = lower.split_once("rss?") else {
        return false;
    };
    query.split(';').any(|part| part == "download")
}
