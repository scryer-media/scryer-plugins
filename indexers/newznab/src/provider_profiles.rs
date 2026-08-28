use std::collections::{BTreeMap, BTreeSet};

use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldType, IndexerProviderEndpointAlias,
    IndexerProviderProfile, IndexerResponseFeatures, PluginNewznabProfile,
    PluginNewznabResponseAttributeMapping, PluginProviderProfile, PluginScoringPolicy,
};
use serde::Deserialize;

const PROFILE_SCHEMA_VERSION: u32 = 1;
const PROFILE_ASSET: &str = include_str!("../known_newznab_profiles.v1.jsonl");

#[derive(Debug, Deserialize)]
struct ProfileRow {
    schema_version: u32,
    id: String,
    display_name: String,
    #[serde(default)]
    legacy_provider_type_aliases: Vec<String>,
    canonical_api_base_url: String,
    api_path: String,
    #[serde(default)]
    endpoint_aliases: Vec<IndexerProviderEndpointAlias>,
    authentication: Authentication,
    limits: Limits,
    #[serde(default)]
    default_request_parameters: BTreeMap<String, String>,
    #[serde(default)]
    allowed_response_formats: Vec<String>,
    #[serde(default)]
    response_attributes: Vec<PluginNewznabResponseAttributeMapping>,
    #[serde(default)]
    response_features: Vec<String>,
    #[serde(default)]
    quirks: Vec<String>,
    #[serde(default)]
    scoring_policy_ids: Vec<String>,
    provenance_url: String,
    reviewed_on: String,
}

#[derive(Debug, Deserialize)]
struct Authentication {
    kind: String,
    query_parameter: String,
}

#[derive(Debug, Deserialize)]
struct Limits {
    request_start_interval_ms: u64,
    #[serde(default)]
    hourly_requests: Option<u32>,
    #[serde(default)]
    daily_requests: Option<u32>,
    retry: Retry,
    page_size: u32,
    #[serde(default)]
    page_ceiling: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Retry {
    max_attempts: u32,
    default_delay_ms: u64,
    max_delay_ms: u64,
    total_budget_ms: u64,
}

pub(super) fn load() -> (Vec<IndexerProviderProfile>, IndexerResponseFeatures) {
    parse(PROFILE_ASSET).expect("bundled Newznab provider profiles must be valid")
}

pub(super) fn selector(profiles: &[IndexerProviderProfile]) -> ConfigFieldDef {
    let mut options = vec![ConfigFieldOption {
        value: "custom".to_string(),
        label: "Custom".to_string(),
        config_overrides: BTreeMap::new(),
    }];
    options.extend(profiles.iter().map(|definition| {
        let PluginProviderProfile::Newznab(profile) = &definition.runtime_profile;
        let mut config_overrides = BTreeMap::from([
            ("profile_id".to_string(), profile.profile_id.clone()),
            ("base_url".to_string(), profile.canonical_base_url.clone()),
            ("api_path".to_string(), profile.api_path.clone()),
            (
                "api_key_parameter".to_string(),
                profile.api_key_parameter.clone(),
            ),
            (
                "request_interval_ms".to_string(),
                profile.request_interval_ms.to_string(),
            ),
            (
                "retry_default_delay_ms".to_string(),
                profile.retry_default_ms.to_string(),
            ),
            (
                "retry_max_delay_ms".to_string(),
                profile.retry_max_ms.to_string(),
            ),
            (
                "retry_max_attempts".to_string(),
                profile.retry_max_attempts.to_string(),
            ),
            (
                "retry_total_budget_ms".to_string(),
                profile.retry_total_budget_ms.to_string(),
            ),
            ("page_size".to_string(), profile.page_size.to_string()),
        ]);
        if let Some(limit) = profile.hourly_limit {
            config_overrides.insert("hourly_request_limit".to_string(), limit.to_string());
        }
        if let Some(limit) = profile.daily_limit {
            config_overrides.insert("daily_request_limit".to_string(), limit.to_string());
        }
        if let Some(limit) = profile.page_ceiling {
            config_overrides.insert("max_pages".to_string(), limit.to_string());
        }
        if !profile.default_request_parameters.is_empty() {
            config_overrides.insert(
                "additional_params".to_string(),
                newznab_common::encode_request_parameters(&profile.default_request_parameters),
            );
        }
        ConfigFieldOption {
            value: profile.profile_id.clone(),
            label: definition.display_name.clone(),
            config_overrides,
        }
    }));

    ConfigFieldDef {
        key: "profile_id".to_string(),
        label: "Known provider".to_string(),
        field_type: ConfigFieldType::Select,
        required: false,
        default_value: None,
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options,
        help_text: Some(
            "Use a known provider preset, or Custom for another Newznab-compatible service."
                .to_string(),
        ),
    }
}

pub(super) fn scoring_policies(profiles: &[IndexerProviderProfile]) -> Vec<PluginScoringPolicy> {
    let selected = profiles
        .iter()
        .flat_map(|definition| {
            let PluginProviderProfile::Newznab(profile) = &definition.runtime_profile;
            profile.scoring_policy_ids.iter().map(String::as_str)
        })
        .collect::<BTreeSet<_>>();
    policy_registry()
        .into_iter()
        .filter(|policy| selected.contains(policy.name.as_str()))
        .collect()
}

fn policy_registry() -> Vec<PluginScoringPolicy> {
    vec![
        PluginScoringPolicy {
            name: "nzbgeek_vote_penalty".to_string(),
            rego_source: REGO_NZBGEEK_VOTE_PENALTY.to_string(),
            applied_facets: vec![],
        },
        PluginScoringPolicy {
            name: "nzbgeek_language_bonus".to_string(),
            rego_source: REGO_NZBGEEK_LANGUAGE_BONUS.to_string(),
            applied_facets: vec![],
        },
        PluginScoringPolicy {
            name: "dognzb_rating_bonus".to_string(),
            rego_source: REGO_DOGNZB_RATING_BONUS.to_string(),
            applied_facets: vec![],
        },
    ]
}

fn parse(source: &str) -> Result<(Vec<IndexerProviderProfile>, IndexerResponseFeatures), String> {
    let mut profiles = Vec::new();
    let mut response_features = IndexerResponseFeatures::default();
    let mut ids = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: ProfileRow = serde_json::from_str(line)
            .map_err(|error| format!("profile row {} is invalid: {error}", index + 1))?;
        validate(&row, index + 1, &mut ids, &mut aliases)?;
        merge_response_features(&mut response_features, &row.response_features, index + 1)?;
        profiles.push(IndexerProviderProfile {
            schema_version: row.schema_version,
            display_name: row.display_name,
            legacy_provider_type_aliases: row.legacy_provider_type_aliases,
            endpoint_aliases: row.endpoint_aliases,
            runtime_profile: PluginProviderProfile::Newznab(PluginNewznabProfile {
                profile_id: row.id,
                canonical_base_url: row.canonical_api_base_url,
                api_path: row.api_path,
                api_key_parameter: row.authentication.query_parameter,
                request_interval_ms: row.limits.request_start_interval_ms,
                hourly_limit: row.limits.hourly_requests,
                daily_limit: row.limits.daily_requests,
                retry_default_ms: row.limits.retry.default_delay_ms,
                retry_max_ms: row.limits.retry.max_delay_ms,
                retry_max_attempts: row.limits.retry.max_attempts,
                retry_total_budget_ms: row.limits.retry.total_budget_ms,
                page_size: row.limits.page_size,
                page_ceiling: row.limits.page_ceiling,
                default_request_parameters: row.default_request_parameters,
                allowed_response_formats: row.allowed_response_formats,
                response_attribute_mappings: row.response_attributes,
                quirks: row.quirks,
                scoring_policy_ids: row.scoring_policy_ids,
            }),
            provenance_url: row.provenance_url,
            reviewed_on: row.reviewed_on,
        });
    }
    if profiles.is_empty() {
        return Err("provider profile asset is empty".to_string());
    }
    Ok((profiles, response_features))
}

fn merge_response_features(
    combined: &mut IndexerResponseFeatures,
    features: &[String],
    line: usize,
) -> Result<(), String> {
    for feature in features {
        match feature.as_str() {
            "languages" => combined.languages = true,
            "subtitles" => combined.subtitles = true,
            "grabs" => combined.grabs = true,
            "votes" => combined.votes = true,
            "comments" => combined.comments = true,
            "info_url" => combined.info_url = true,
            "guid" => combined.guid = true,
            "raw_provider_metadata" => combined.raw_provider_metadata = true,
            "password_hint" => combined.password_hint = true,
            "protection_hint" => combined.protection_hint = true,
            _ => {
                return Err(format!(
                    "profile row {line} has unknown response feature '{feature}'"
                ));
            }
        }
    }
    Ok(())
}

fn validate(
    row: &ProfileRow,
    line: usize,
    ids: &mut BTreeSet<String>,
    aliases: &mut BTreeSet<String>,
) -> Result<(), String> {
    if row.schema_version != PROFILE_SCHEMA_VERSION {
        return Err(format!(
            "profile row {line} uses unsupported schema version {}",
            row.schema_version
        ));
    }
    if row.id.trim().is_empty() || !ids.insert(row.id.clone()) {
        return Err(format!("profile row {line} has an empty or duplicate id"));
    }
    for alias in &row.legacy_provider_type_aliases {
        if alias.trim().is_empty() || !aliases.insert(alias.clone()) {
            return Err(format!(
                "profile row {line} has an empty or duplicate legacy alias"
            ));
        }
    }
    if row.authentication.kind != "api_key_query"
        || row.authentication.query_parameter.trim().is_empty()
    {
        return Err(format!("profile row {line} has invalid authentication"));
    }
    if !row.canonical_api_base_url.starts_with("https://")
        || !row.api_path.starts_with('/')
        || row.limits.request_start_interval_ms == 0
        || row.limits.page_size == 0
        || row.limits.retry.max_attempts == 0
        || row.limits.retry.default_delay_ms > row.limits.retry.max_delay_ms
        || row.provenance_url.trim().is_empty()
        || row.reviewed_on.trim().is_empty()
    {
        return Err(format!("profile row {line} contains invalid defaults"));
    }
    if row
        .response_features
        .iter()
        .any(|value| value.trim().is_empty())
    {
        return Err(format!("profile row {line} has an empty response feature"));
    }
    let known_policy_ids = policy_registry()
        .into_iter()
        .map(|policy| policy.name)
        .collect::<BTreeSet<_>>();
    if row
        .scoring_policy_ids
        .iter()
        .any(|policy_id| !known_policy_ids.contains(policy_id))
    {
        return Err(format!(
            "profile row {line} references an unknown scoring policy"
        ));
    }
    Ok(())
}

const REGO_NZBGEEK_VOTE_PENALTY: &str = r#"package scryer.rules.user.plugin_nzbgeek_vote_penalty
import rego.v1

score_entry["nzbgeek_thumbs_down"] := penalty if {
    input.release.extra.newznab_profile_id == "nzbgeek"
    td := input.release.extra.thumbs_down
    td > 5
    extra := min([td - 5, 10])
    penalty := -2400 - (extra * 300)
}
"#;

const REGO_NZBGEEK_LANGUAGE_BONUS: &str = r#"package scryer.rules.user.plugin_nzbgeek_language_bonus
import rego.v1

score_entry["nzbgeek_english_confirmed"] := 200 if {
    input.release.extra.newznab_profile_id == "nzbgeek"
    some lang in input.release.languages_audio
    lower(lang) == "eng"
}
"#;

const REGO_DOGNZB_RATING_BONUS: &str = r#"package scryer.rules.user.plugin_dognzb_rating_bonus
import rego.v1

score_entry["dognzb_high_rating"] := 150 if {
    input.release.extra.newznab_profile_id == "dognzb"
    input.release.extra.rating >= 80
}

score_entry["dognzb_mid_rating"] := 50 if {
    input.release.extra.newznab_profile_id == "dognzb"
    input.release.extra.rating >= 60
    input.release.extra.rating < 80
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_profiles_are_typed_unique_and_credential_free() {
        let (profiles, response_features) = load();
        assert_eq!(profiles.len(), 2);
        assert!(response_features.languages);
        assert!(response_features.subtitles);
        assert!(response_features.votes);
        assert!(!PROFILE_ASSET.to_ascii_lowercase().contains("api_key\":"));

        let ids = profiles
            .iter()
            .map(|definition| {
                let PluginProviderProfile::Newznab(profile) = &definition.runtime_profile;
                profile.profile_id.as_str()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from(["dognzb", "nzbgeek"]));
    }

    #[test]
    fn selector_prefills_profile_defaults_without_credentials() {
        let (profiles, _) = load();
        let field = selector(&profiles);
        assert_eq!(field.key, "profile_id");
        assert!(!field.required);
        assert!(field.default_value.is_none());
        let option = field
            .options
            .iter()
            .find(|option| option.value == "nzbgeek")
            .expect("known profile option");
        assert_eq!(option.label, "NZBGeek");
        assert_eq!(
            option.config_overrides.get("base_url").map(String::as_str),
            Some("https://api.nzbgeek.info")
        );
        assert_eq!(
            option
                .config_overrides
                .get("additional_params")
                .map(String::as_str),
            Some("extended=1")
        );
        assert!(!option.config_overrides.contains_key("api_key"));
    }

    #[test]
    fn catalog_selects_only_guarded_canonical_scoring_policies() {
        let (profiles, _) = load();
        let policies = scoring_policies(&profiles);
        assert_eq!(policies.len(), 3);
        for policy in &policies {
            assert!(policy.rego_source.contains("newznab_profile_id"));
        }
        let language = policies
            .iter()
            .find(|policy| policy.name == "nzbgeek_language_bonus")
            .expect("language policy");
        assert!(
            language
                .rego_source
                .contains("input.release.languages_audio")
        );
        assert!(
            !language
                .rego_source
                .contains("input.release.extra.languages")
        );
    }
}
