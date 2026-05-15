ProviderDescriptor::Indexer(IndexerDescriptor {
    provider_type: "{{plugin_id}}".to_string(),
    provider_aliases: vec![],
    source_kind: IndexerSourceKind::Generic,
    capabilities: IndexerCapabilities::default(),
    scoring_policies: vec![],
    config_fields: vec![],
    allowed_hosts: vec![],
    rate_limit_seconds: None,
})
