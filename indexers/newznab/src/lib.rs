use std::collections::HashMap;

use newznab_common::{
    Capabilities, IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor,
    IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol, IndexerSearchInput,
    IndexerSourceKind, NewznabConfig, PluginActionRequest, PluginActionResponse, PluginDescriptor,
    ProviderDescriptor, SDK_VERSION, SearchRequest, SearchResponse, current_sdk_constraint,
    execute_full_search, extract_profile_metadata, standard_config_fields,
};
use scryer_plugin_pdk::*;

mod provider_profiles;

fn build_descriptor() -> PluginDescriptor {
    let (provider_profiles, response_features) = provider_profiles::load();
    let scoring_policies = provider_profiles::scoring_policies(&provider_profiles);
    let mut config_fields = standard_config_fields(None);
    if let Some(base_url) = config_fields
        .iter_mut()
        .find(|field| field.key == "base_url")
    {
        base_url.required = false;
        base_url.help_text = Some(
            "Required for Custom; known providers use their bundled API endpoint unless overridden."
                .to_string(),
        );
    }
    config_fields.insert(0, provider_profiles::selector(&provider_profiles));
    PluginDescriptor {
        id: "newznab".to_string(),
        name: "Newznab Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "newznab".to_string(),
            provider_aliases: vec!["nzbgeek".to_string(), "dognzb".to_string()],
            provider_profiles,
            search_semantics_version: Some(2),
            strategy_plan: Some(scryer_plugin_sdk::IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 4,
            }),
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
                response_features: Some(response_features),
            },
            scoring_policies,
            config_fields,
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    }
}

async fn search(req: SearchRequest) -> Result<SearchResponse, Error> {
    let config = NewznabConfig::from_host()?;
    let response = execute_full_search(&config, &req, extract_profile_metadata).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_owns_profiles_selector_and_guarded_policies() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };
        assert_eq!(indexer.provider_profiles.len(), 2);
        assert_eq!(indexer.scoring_policies.len(), 3);
        assert!(
            indexer
                .scoring_policies
                .iter()
                .all(|policy| policy.rego_source.contains("newznab_profile_id"))
        );
        let selector = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "profile_id")
            .expect("provider selector");
        assert_eq!(selector.options.len(), 3);
        let base_url = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "base_url")
            .expect("base URL field");
        assert!(!base_url.required);
    }
}
