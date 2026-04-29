use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    execute_full_search, extract_base_metadata, standard_config_fields, Capabilities,
    IndexerDescriptor, IndexerSourceKind, NewznabConfig, PluginDescriptor, PluginResult,
    ProviderDescriptor, SDK_VERSION, SearchRequest,
};

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "newznab".to_string(),
        name: "Newznab Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "newznab".to_string(),
            provider_aliases: vec![],
            source_kind: IndexerSourceKind::Usenet,
            capabilities: Capabilities {
                supported_ids: HashMap::from([
                    ("movie".into(), vec!["imdb_id".into()]),
                    ("series".into(), vec!["tvdb_id".into()]),
                    ("anime".into(), vec!["tvdb_id".into()]),
                ]),
                deduplicates_aliases: false,
                season_param: Some("season".into()),
                episode_param: Some("ep".into()),
                query_param: Some("q".into()),
                search: true,
                imdb_search: true,
                tvdb_search: true,
                anidb_search: false,
                rss: true,
            },
            scoring_policies: vec![],
            config_fields: standard_config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let config = NewznabConfig::from_extism()?;
    let response = execute_full_search(&config, &req, extract_base_metadata)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
