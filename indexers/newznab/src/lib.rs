use std::collections::HashMap;

use newznab_common::{
    Capabilities, IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor,
    IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures,
    IndexerSearchInput, IndexerSourceKind, NewznabConfig, PluginActionRequest,
    PluginActionResponse, PluginDescriptor, ProviderDescriptor, SDK_VERSION, SearchRequest,
    SearchResponse, current_sdk_constraint, execute_full_search, extract_base_metadata,
    standard_config_fields,
};
use scryer_plugin_pdk::*;

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "newznab".to_string(),
        name: "Newznab Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "newznab".to_string(),
            provider_aliases: vec![],
            search_semantics_version: Some(1),
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
                supported_query_facets: vec![],
                search: true,
                imdb_search: true,
                tvdb_search: true,
                anidb_search: false,
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
                supported_external_ids: vec![
                    "imdb_id".into(),
                    "tvdb_id".into(),
                    "tmdb_id".into(),
                    "tvmaze_id".into(),
                    "tvrage_id".into(),
                ],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(100),
                    max_page_size: Some(100),
                    max_pages: Some(30),
                    api_quota_supported: true,
                    grab_quota_supported: true,
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: None,
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    grabs: true,
                    comments: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    password_hint: true,
                    protection_hint: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: standard_config_fields(None),
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    }
}

async fn search(req: SearchRequest) -> Result<SearchResponse, Error> {
    let config = NewznabConfig::from_host()?;
    let response = execute_full_search(&config, &req, extract_base_metadata).await?;
    Ok(response)
}

async fn action(request: PluginActionRequest) -> Result<PluginActionResponse, Error> {
    newznab_common::execute_provider_action(request).await
}

scryer_plugin_pdk::scryer_indexer_component_main!(
    descriptor = build_descriptor,
    search = search,
    action = action,
);
