ProviderDescriptor::ArchiveExtractor(ArchiveExtractorDescriptor {
    provider_type: "{{plugin_id}}".to_string(),
    provider_aliases: vec![],
    config_fields: vec![],
    default_base_url: None,
    allowed_hosts: vec![],
    capabilities: ArchiveExtractorCapabilities {
        formats: vec![],
        repair_formats: vec![],
    },
})
