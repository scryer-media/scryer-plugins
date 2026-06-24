use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    Capabilities, IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol,
    IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind, NewznabConfig,
    PluginDescriptor, PluginResult, ProviderDescriptor, SDK_VERSION, SearchRequest,
    current_sdk_constraint, execute_full_search, extract_base_metadata, standard_config_fields,
};

const ANIMETOSHO_ARCHIVE_BASE_URL: &str = "https://feed.animetosho.org";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "animetosho-archive".to_string(),
        name: "AnimeTosho Archive Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(newznab_common::IndexerDescriptor {
            provider_type: "animetosho-archive".to_string(),
            provider_aliases: vec![],
            source_kind: IndexerSourceKind::Usenet,
            capabilities: Capabilities {
                supported_ids: HashMap::new(),
                deduplicates_aliases: false,
                season_param: None,
                episode_param: None,
                query_param: Some("q".into()),
                supported_query_facets: vec!["anime".into()],
                search: true,
                imdb_search: false,
                tvdb_search: false,
                anidb_search: false,
                rss: false,
                protocols: vec![IndexerProtocol::Usenet],
                feed_modes: vec![
                    IndexerFeedMode::AutomaticSearch,
                    IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![IndexerSearchInput::TitleQuery, IndexerSearchInput::Limit],
                supported_external_ids: vec![],
                category_model: None,
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(200),
                    max_page_size: Some(200),
                    max_pages: Some(10),
                    api_quota_supported: true,
                    grab_quota_supported: false,
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: None,
                response_features: Some(IndexerResponseFeatures {
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec![],
            rate_limit_seconds: Some(2),
        }),
    }
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let mut req: SearchRequest = serde_json::from_str(&input)?;
    normalize_archive_request(&mut req);
    let config = archive_config_from_extism()?;
    let response = execute_full_search(&config, &req, extract_base_metadata)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn config_fields() -> Vec<newznab_common::ConfigFieldDef> {
    let mut fields = standard_config_fields(Some(ANIMETOSHO_ARCHIVE_BASE_URL));
    for field in &mut fields {
        match field.key.as_str() {
            "base_url" => {
                field.help_text = Some("AnimeTosho feed API base URL".to_string());
            }
            "api_key" => {
                field.required = false;
                field.default_value = Some("0".to_string());
                field.help_text = Some("AnimeTosho's public feed API uses key 0".to_string());
            }
            "additional_params" => {
                field.help_text = Some(
                    "Extra query parameters appended to every archive search request".to_string(),
                );
            }
            _ => {}
        }
    }
    fields
}

fn archive_config_from_extism() -> Result<NewznabConfig, Error> {
    Ok(archive_config(
        config_string("base_url")?.unwrap_or_else(|| ANIMETOSHO_ARCHIVE_BASE_URL.to_string()),
        config_string("api_key")?.unwrap_or_else(|| "0".to_string()),
        config_string("api_path")?.unwrap_or_else(|| "/api".to_string()),
        config_string("additional_params")?.unwrap_or_default(),
        configured_page_size().unwrap_or(200),
    ))
}

fn archive_config(
    base_url: String,
    api_key: String,
    api_path: String,
    additional_params: String,
    requested_page_size: usize,
) -> NewznabConfig {
    NewznabConfig {
        base_url: base_url.trim().to_string(),
        api_key: api_key.trim().to_string(),
        api_path: api_path.trim().to_string(),
        additional_params: additional_params.trim().to_string(),
        page_size: requested_page_size.clamp(1, 200),
    }
}

fn normalize_archive_request(req: &mut SearchRequest) {
    req.ids.clear();
    req.facet = None;
    req.category = None;
    req.categories.clear();
    req.season = None;
    req.episode = None;
    req.absolute_episode = None;
}

fn config_string(key: &str) -> Result<Option<String>, Error> {
    Ok(config::get(key)
        .map_err(|e| Error::msg(format!("failed to read config {key}: {e}")))?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn configured_page_size() -> Option<usize> {
    config::get("page_size")
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_id_free_and_anime_text_capable_for_scryer_dispatch() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        assert!(indexer.capabilities.supported_ids.is_empty());
        assert_eq!(indexer.capabilities.query_param.as_deref(), Some("q"));
        assert_eq!(
            indexer.capabilities.supported_query_facets,
            vec!["anime".to_string()]
        );
        assert!(!indexer.capabilities.tvdb_search);
        assert!(!indexer.capabilities.rss);

        let api_key = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "api_key")
            .expect("api_key field");
        assert!(!api_key.required);
        assert_eq!(api_key.default_value.as_deref(), Some("0"));
    }

    #[test]
    fn archive_config_defaults_to_public_feed_api() {
        let config = archive_config(
            ANIMETOSHO_ARCHIVE_BASE_URL.to_string(),
            "0".to_string(),
            "/api".to_string(),
            "&extended=1".to_string(),
            500,
        );

        assert_eq!(config.base_url, ANIMETOSHO_ARCHIVE_BASE_URL);
        assert_eq!(config.api_key, "0");
        assert_eq!(config.api_path, "/api");
        assert_eq!(config.additional_params, "&extended=1");
        assert_eq!(config.page_size, 200);
    }

    #[test]
    fn normalizes_scryer_anime_request_to_generic_archive_query() {
        let mut request = SearchRequest {
            query: "Naruto Shippuden 163".to_string(),
            facet: Some("anime".to_string()),
            category: Some("anime".to_string()),
            categories: vec!["5070".to_string()],
            season: Some(8),
            episode: Some(163),
            absolute_episode: Some(163),
            ..SearchRequest::default()
        };
        request
            .ids
            .insert("tvdb_id".to_string(), "79824".to_string());
        request
            .ids
            .insert("anidb_id".to_string(), "4880".to_string());

        normalize_archive_request(&mut request);

        assert_eq!(request.query, "Naruto Shippuden 163");
        assert!(request.ids.is_empty());
        assert!(request.facet.is_none());
        assert!(request.category.is_none());
        assert!(request.categories.is_empty());
        assert!(request.season.is_none());
        assert!(request.episode.is_none());
        assert!(request.absolute_episode.is_none());
    }
}
