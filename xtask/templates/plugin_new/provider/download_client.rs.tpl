ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
    provider_type: "{{plugin_id}}".to_string(),
    provider_aliases: vec![],
    config_fields: vec![],
    default_base_url: None,
    allowed_hosts: vec![],
    accepted_inputs: vec![],
    isolation_modes: vec![],
    capabilities: DownloadClientCapabilities::default(),
})
