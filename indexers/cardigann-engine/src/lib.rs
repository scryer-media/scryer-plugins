//! Cardigann v11 tracker definitions as a Scryer WASIp2 indexer component.
//!
//! The engine itself (definition parsing, the Go-template renderer, the filter
//! chain, and the search/login/grab flow machine) is host-independent: it is a
//! pure state machine that asks for one HTTP attempt at a time. This module is
//! the whole ABI boundary — descriptor, configuration, and the async component
//! `search`/`action` exports.

use std::collections::{BTreeMap, HashSet};

use scryer_plugin_pdk::{Error, FnResult, component, sdk};
use scryer_plugin_sdk::command::{PluginActionRequest, PluginActionResponse};
use serde_json::Value;
use url::Url;

mod definition;
mod filters;
mod runtime;
mod template;

use definition::{Definition, Setting};
use runtime::EngineAction;

const CARDIGANN_DEFINITION_VERSION: u16 = 11;
const HOST_CONFIG_KEYS: [&str; 5] = [
    "base_url",
    "username",
    "password",
    "cookie",
    "cardigannCaptcha",
];

fn build_descriptor() -> sdk::PluginDescriptor {
    sdk::PluginDescriptor {
        id: "cardigann-engine".to_string(),
        name: "Cardigann Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: sdk::SDK_VERSION.to_string(),
        sdk_constraint: sdk::current_sdk_constraint(),
        socket_permissions: vec![],
        provider: sdk::ProviderDescriptor::Indexer(sdk::IndexerDescriptor {
            provider_type: "cardigann".to_string(),
            provider_aliases: vec![],
            // The operator supplies the definition; there is nothing generic to
            // preset, and a profile must never assert provider facts the pasted
            // definition contradicts.
            provider_profiles: vec![],
            // Cardigann definitions describe arbitrary third-party trackers, so
            // this engine cannot attest that an empty or truncated page means
            // the tracker had nothing more. Leaving this unset keeps the host
            // from crediting convergence coverage to those responses.
            search_semantics_version: None,
            // Deliberately no strategy plan: one configured Cardigann indexer
            // shares a single login session and the definition's `requestdelay`,
            // so fanning several strategies out inside one invocation would race
            // concurrent logins against one cookie jar. The host runs the tiers
            // sequentially instead.
            strategy_plan: None,
            source_kind: sdk::IndexerSourceKind::Torrent,
            capabilities: sdk::IndexerCapabilities {
                rss: true,
                search: true,
                imdb_search: true,
                tvdb_search: true,
                anidb_search: true,
                query_param: Some("query".to_string()),
                protocols: vec![sdk::IndexerProtocol::Torrent],
                feed_modes: vec![
                    sdk::IndexerFeedMode::Recent,
                    sdk::IndexerFeedMode::Rss,
                    sdk::IndexerFeedMode::AutomaticSearch,
                    sdk::IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![
                    sdk::IndexerSearchInput::TextQuery,
                    sdk::IndexerSearchInput::Category,
                    sdk::IndexerSearchInput::Offset,
                    sdk::IndexerSearchInput::Limit,
                ],
                ..sdk::IndexerCapabilities::default()
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            // The component host scopes transport to the configured connection
            // URL. Definition links are configuration, not static plugin
            // allowlist entries.
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    }
}

fn config_fields() -> Vec<sdk::ConfigFieldDef> {
    vec![
        field(
            "base_url",
            "Base URL",
            sdk::ConfigFieldType::String,
            true,
            Some(sdk::ConfigFieldRole::ConnectionUrl),
            Some("The tracker URL selected from the definition's links."),
        ),
        field(
            "definition_yaml",
            "Cardigann Definition",
            sdk::ConfigFieldType::Multiline,
            true,
            None,
            Some("A Prowlarr Cardigann v11 YAML definition."),
        ),
        field(
            "extra_field_data_json",
            "Definition Settings JSON",
            sdk::ConfigFieldType::Multiline,
            false,
            None,
            Some("JSON object for definition-specific Cardigann settings."),
        ),
        field(
            "username",
            "Username",
            sdk::ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "password",
            "Password",
            sdk::ConfigFieldType::Password,
            false,
            None,
            None,
        ),
        field(
            "cookie",
            "Cookie",
            sdk::ConfigFieldType::Password,
            false,
            None,
            Some("Optional initial tracker cookie."),
        ),
        field(
            "cardigannCaptcha",
            "CAPTCHA Answer",
            sdk::ConfigFieldType::String,
            false,
            None,
            Some("Answer returned for a Cardigann image CAPTCHA."),
        ),
    ]
}

fn field(
    key: &str,
    label: &str,
    field_type: sdk::ConfigFieldType,
    required: bool,
    role: Option<sdk::ConfigFieldRole>,
    help_text: Option<&str>,
) -> sdk::ConfigFieldDef {
    sdk::ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: None,
        value_source: sdk::ConfigFieldValueSource::User,
        role,
        host_binding: None,
        options: vec![],
        help_text: help_text.map(str::to_string),
    }
}

async fn search(request: sdk::PluginSearchRequest) -> FnResult<sdk::PluginSearchResponse> {
    let (definition, config) = load_definition_and_config().map_err(Error::msg)?;
    runtime::search(definition, request, config)
        .await
        .map_err(Error::msg)
}

/// The component's named-action surface.
///
/// `checkCaptcha` runs the definition's login flow far enough to fetch the
/// image challenge and hands the operator its bytes; the answer comes back in
/// the `cardigannCaptcha` setting. `grab` runs the definition's authenticated
/// download flow for one release URL, which is how Scryer's download router
/// obtains an artifact a tracker will not serve to an unauthenticated fetch.
/// Anything else is reported as unsupported so the host can keep its own
/// fallbacks.
async fn action(request: PluginActionRequest) -> FnResult<PluginActionResponse> {
    let action = match request.action.as_str() {
        "checkCaptcha" | "check_captcha" => EngineAction::CheckCaptcha,
        "grab" => EngineAction::Grab(action_url(&request.payload).map_err(Error::msg)?),
        action => {
            return Err(component::structured_plugin_error(
                component::action_unsupported(action),
            ));
        }
    };
    let (definition, config) = load_definition_and_config().map_err(Error::msg)?;
    Ok(PluginActionResponse {
        payload: runtime::action(definition, action, config)
            .await
            .map_err(Error::msg)?,
    })
}

fn action_url(payload: &Value) -> Result<String, String> {
    let url = match payload {
        Value::String(url) => Some(url.as_str()),
        Value::Object(object) => object.get("url").and_then(Value::as_str),
        _ => None,
    }
    .map(str::trim)
    .filter(|url| !url.is_empty())
    .ok_or_else(|| "grab action requires a non-empty URL payload".to_string())?;
    Ok(url.to_string())
}

fn load_definition_and_config() -> Result<(Definition, BTreeMap<String, String>), String> {
    let definition_yaml = required_host_config("definition_yaml")?;
    let definition = parse_definition(&definition_yaml)?;
    let mut config = BTreeMap::new();
    for key in HOST_CONFIG_KEYS {
        if let Some(value) = host_config(key)?
            && !value.trim().is_empty()
        {
            config.insert(key.to_string(), value);
        }
    }
    merge_extra_field_data(&mut config, host_config("extra_field_data_json")?)?;
    if config
        .get("base_url")
        .is_none_or(|base_url| base_url.trim().is_empty())
    {
        return Err("base_url must be configured".to_string());
    }
    validate_configured_base_url(&definition, &config)?;
    Ok((definition, config))
}

fn validate_configured_base_url(
    definition: &Definition,
    config: &BTreeMap<String, String>,
) -> Result<(), String> {
    let configured = config
        .get("base_url")
        .expect("base_url was checked before validation");
    let configured = Url::parse(configured)
        .map_err(|error| format!("invalid base_url `{configured}`: {error}"))?;
    let configured_origin = configured.origin();
    let matches_definition = definition
        .links
        .iter()
        .chain(&definition.legacy_links)
        .map(|link| {
            Url::parse(link).map_err(|error| format!("invalid definition link `{link}`: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .any(|link| link.origin() == configured_origin);
    if matches_definition {
        Ok(())
    } else {
        Err(format!(
            "base_url `{configured}` has an origin not declared by the Cardigann definition"
        ))
    }
}

/// One descriptor-bound configuration value from the component host.
fn host_config(key: &str) -> Result<Option<String>, String> {
    Ok(component::config_get(key))
}

fn required_host_config(key: &str) -> Result<String, String> {
    host_config(key)?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("`{key}` must be configured"))
}

fn merge_extra_field_data(
    config: &mut BTreeMap<String, String>,
    raw: Option<String>,
) -> Result<(), String> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let Value::Object(values) = serde_json::from_str::<Value>(&raw)
        .map_err(|error| format!("invalid extra_field_data_json: {error}"))?
    else {
        return Err("extra_field_data_json must be a JSON object".to_string());
    };
    for (key, value) in values {
        let value = match value {
            Value::String(value) => value,
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::Null => continue,
            Value::Array(_) | Value::Object(_) => {
                return Err(format!(
                    "extra_field_data_json value `{key}` must be a string, number, boolean, or null"
                ));
            }
        };
        config.insert(key, value);
    }
    Ok(())
}

fn parse_definition(source: &str) -> Result<Definition, String> {
    let definition: Definition = serde_yaml::from_str(source.trim_start_matches('\u{feff}'))
        .map_err(|error| {
            format!("invalid Cardigann v{CARDIGANN_DEFINITION_VERSION} definition: {error}")
        })?;
    validate_metadata(&definition)?;
    definition.validate_supported()?;
    Ok(definition)
}

fn validate_metadata(definition: &Definition) -> Result<(), String> {
    if definition.id.trim().is_empty() {
        return Err("definition id must not be empty".to_string());
    }
    if definition.name.trim().is_empty() {
        return Err("definition name must not be empty".to_string());
    }
    if definition.definition_type.trim().is_empty() {
        return Err("definition type must not be empty".to_string());
    }
    if definition.links.is_empty() {
        return Err("definition links must contain at least one base URL".to_string());
    }
    let mut setting_names = HashSet::new();
    for Setting {
        name, setting_type, ..
    } in &definition.settings
    {
        if name.trim().is_empty() {
            return Err("definition setting name must not be empty".to_string());
        }
        if setting_type.trim().is_empty() {
            return Err(format!("setting `{name}` has no type"));
        }
        if !setting_names.insert(name.as_str()) {
            return Err(format!("duplicate definition setting `{name}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod component_abi_tests {
    use super::*;

    const DEFINITION: &str = r#"
id: example
name: Example Tracker
description: Test definition
language: en-US
type: private
encoding: UTF-8
links: [https://tracker.example/path]
legacylinks: [https://legacy.tracker.example/]
caps: {}
search:
  paths:
    - path: /search
"#;

    #[test]
    fn descriptor_is_a_connection_scoped_component_indexer() {
        let descriptor = build_descriptor();
        let sdk::ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };
        assert_eq!(indexer.provider_type, "cardigann");
        assert!(indexer.allowed_hosts.is_empty());
        assert!(indexer.capabilities.search);
        assert_eq!(
            indexer
                .config_fields
                .iter()
                .find(|field| field.key == "base_url")
                .and_then(|field| field.role),
            Some(sdk::ConfigFieldRole::ConnectionUrl)
        );
    }

    #[test]
    fn parser_keeps_cardigann_schema_validation() {
        assert_eq!(parse_definition(DEFINITION).unwrap().id, "example");
    }

    #[test]
    fn extra_fields_override_explicit_values_and_stay_scalar() {
        let mut config = BTreeMap::from([("username".to_string(), "host-user".to_string())]);
        merge_extra_field_data(
            &mut config,
            Some(r#"{"username":"definition-user","apiurl":"api.example","enabled":true}"#.into()),
        )
        .unwrap();
        assert_eq!(config.get("username").unwrap(), "definition-user");
        assert_eq!(config.get("apiurl").unwrap(), "api.example");
        assert_eq!(config.get("enabled").unwrap(), "true");
    }

    #[test]
    fn base_url_must_match_a_declared_definition_origin() {
        let definition = parse_definition(DEFINITION).unwrap();
        let accepted = BTreeMap::from([(
            "base_url".to_string(),
            "https://legacy.tracker.example/alternate".to_string(),
        )]);
        validate_configured_base_url(&definition, &accepted).unwrap();

        let rejected = BTreeMap::from([(
            "base_url".to_string(),
            "https://attacker.example/".to_string(),
        )]);
        let error = validate_configured_base_url(&definition, &rejected).unwrap_err();
        assert!(error.contains("not declared"));
    }

    #[test]
    fn extra_field_base_url_override_cannot_escape_definition_origins() {
        let definition = parse_definition(DEFINITION).unwrap();
        let mut config = BTreeMap::from([(
            "base_url".to_string(),
            "https://tracker.example/".to_string(),
        )]);
        merge_extra_field_data(
            &mut config,
            Some(r#"{"base_url":"https://attacker.example/"}"#.to_string()),
        )
        .unwrap();
        let error = validate_configured_base_url(&definition, &config).unwrap_err();
        assert!(error.contains("attacker.example"));
    }

    #[test]
    fn grab_accepts_the_action_payload_shapes() {
        assert_eq!(
            action_url(&Value::String("https://tracker.example/download".into())).unwrap(),
            "https://tracker.example/download"
        );
        assert_eq!(
            action_url(&serde_json::json!({"url": "https://tracker.example/download"})).unwrap(),
            "https://tracker.example/download"
        );
    }

    #[test]
    fn executes_requested_v11_corpus_to_an_initial_http_step() {
        let corpus_dir = std::env::var_os("CARDIGANN_V11_DEFINITIONS_DIR")
            .or_else(|| std::env::var_os("CARDIGANN_V11_CORPUS_DIR"));
        let Some(corpus_dir) = corpus_dir else {
            return;
        };

        let mut definitions = std::fs::read_dir(corpus_dir)
            .expect("read Cardigann v11 corpus")
            .map(|entry| entry.expect("read corpus entry").path())
            .filter(|path| {
                matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("yml" | "yaml")
                )
            })
            .collect::<Vec<_>>();
        definitions.sort();
        assert!(!definitions.is_empty(), "Cardigann v11 corpus is empty");

        let total = definitions.len();
        let mut passed = 0usize;
        let mut failures = Vec::new();
        for path in definitions {
            let source = match std::fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) => {
                    failures.push(format!("{}: could not read: {error}", path.display()));
                    continue;
                }
            };
            let definition = match parse_definition(&source) {
                Ok(definition) => definition,
                Err(error) => {
                    failures.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let Some(base_url) = definition.links.first().cloned() else {
                failures.push(format!("{}: definition has no base URL", path.display()));
                continue;
            };
            let compiled_ir = match serde_json::to_string(&definition::CompiledIr {
                ir_version: definition::COMPILED_IR_VERSION,
                definition,
            }) {
                Ok(compiled_ir) => compiled_ir,
                Err(error) => {
                    failures.push(format!("{}: could not encode IR: {error}", path.display()));
                    continue;
                }
            };
            match runtime::begin(
                compiled_ir,
                runtime::Operation::Search(Box::default()),
                BTreeMap::from([("base_url".to_string(), base_url)]),
            ) {
                Ok(runtime::Step::NeedHttp { .. }) => passed += 1,
                Ok(step) => failures.push(format!(
                    "{}: flow did not begin with HTTP: {step:?}",
                    path.display()
                )),
                Err(error) => {
                    failures.push(format!("{}: flow start failed: {error}", path.display()))
                }
            }
        }

        let failed = failures.len();
        eprintln!("Cardigann v11 corpus: total={total}, passed={passed}, failed={failed}");
        assert!(
            failures.is_empty(),
            "Cardigann v11 corpus: total={total}, passed={passed}, failed={failed}\n{}",
            failures.join("\n")
        );
    }
}

scryer_plugin_pdk::scryer_indexer_component_main!(
    descriptor = build_descriptor,
    search = search,
    action = action,
);
