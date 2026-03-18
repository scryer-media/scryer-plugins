use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    execute_full_search, extract_base_metadata, standard_config_fields, Capabilities,
    NewznabConfig, PluginDescriptor, SearchRequest,
};

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        name: "Newznab Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "usenet_indexer".to_string(),
        provider_type: "newznab".to_string(),
        provider_aliases: vec!["dognzb".to_string()],
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
        },
        scoring_policies: vec![],
        config_fields: standard_config_fields(),
        allowed_hosts: vec![],
        rate_limit_seconds: None,
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let config = NewznabConfig::from_extism()?;
    let response = execute_full_search(&config, &req, extract_base_metadata)?;
    Ok(serde_json::to_string(&response)?)
}
