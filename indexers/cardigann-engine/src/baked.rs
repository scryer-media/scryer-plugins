//! The Cardigann definitions this plugin ships with.
//!
//! Operators should not have to paste 3.5 MB of third-party YAML into a text
//! box to add a tracker, so the whole gate-passing Prowlarr v11 corpus is baked
//! into the component and offered through a `definition` selector, exactly the
//! way `indexers/newznab` offers its known providers. Refreshing the corpus is
//! then a CI job that reruns `xtask cardigann sync-definitions` and opens a PR.
//!
//! The asset is two committed files rather than one so that reading it stays
//! cheap on both hot paths:
//!
//! * [`INDEX_ASSET`] is the small line-per-definition index — id, label, and
//!   the definition's canonical base URL. `describe` parses all of it to build
//!   the selector, and it is the only part a descriptor build touches.
//! * [`BODIES_ASSET`] holds one line per definition carrying the YAML itself.
//!   Loading a configured indexer scans it for that one line and parses only
//!   that line, so the 3.5 MB of YAML is never decoded wholesale.
//!
//! Both files are generated; `id` is always the first field on a body line,
//! which is what makes the single-line lookup a prefix match.

use std::collections::{BTreeMap, BTreeSet};

use scryer_plugin_pdk::sdk;
use serde::{Deserialize, Serialize};

/// Bumped when the row shape changes, so a stale asset fails loudly instead of
/// silently deserializing into the wrong thing.
pub const ASSET_SCHEMA_VERSION: u32 = 1;

/// The selector value that keeps the paste-in escape hatch available.
pub const CUSTOM_DEFINITION_ID: &str = "custom";

pub const INDEX_ASSET: &str = include_str!("../known_cardigann_definitions.v1.jsonl");
pub const BODIES_ASSET: &str = include_str!("../known_cardigann_definitions.v1.bodies.jsonl");

/// One index row. Field order here is the field order on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRow {
    pub schema_version: u32,
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub base_url: String,
}

/// One body row. `id` must stay first: the lookup is a prefix match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyRow {
    pub id: String,
    pub definition_yaml: String,
}

/// The baked index, or an error describing the first malformed row.
pub fn index() -> Result<Vec<IndexRow>, String> {
    parse_index(INDEX_ASSET)
}

/// The `definition` selector: `Custom` plus one option per baked definition.
///
/// Options carry only a `base_url` prefill. A whole definition never travels in
/// `config_overrides`; the component reads it out of [`BODIES_ASSET`] once the
/// operator has chosen an id.
pub fn selector(rows: &[IndexRow]) -> sdk::ConfigFieldDef {
    let mut options = vec![sdk::ConfigFieldOption {
        value: CUSTOM_DEFINITION_ID.to_string(),
        label: "Custom (paste a definition)".to_string(),
        config_overrides: BTreeMap::new(),
    }];
    options.extend(rows.iter().map(|row| sdk::ConfigFieldOption {
        value: row.id.clone(),
        label: option_label(row),
        config_overrides: BTreeMap::from([("base_url".to_string(), row.base_url.clone())]),
    }));
    sdk::ConfigFieldDef {
        key: "definition".to_string(),
        label: "Tracker Definition".to_string(),
        field_type: sdk::ConfigFieldType::Select,
        required: false,
        default_value: None,
        value_source: sdk::ConfigFieldValueSource::User,
        role: None,
        host_binding: None,
        options,
        help_text: Some(
            "A bundled Cardigann v11 tracker definition. Choose Custom to paste your own \
             definition into the Cardigann Definition field instead."
                .to_string(),
        ),
    }
}

fn option_label(row: &IndexRow) -> String {
    if row.kind.trim().is_empty() {
        row.display_name.clone()
    } else {
        format!("{} ({})", row.display_name, row.kind)
    }
}

/// The YAML for one baked definition, or `None` when the id is not baked.
///
/// Only the matching line is deserialized. Every other line is skipped with a
/// byte-prefix comparison, so this stays cheap over the full corpus.
pub fn definition_yaml(id: &str) -> Result<Option<String>, String> {
    let Some(line) = find_body_line(BODIES_ASSET, id) else {
        return Ok(None);
    };
    let row: BodyRow = serde_json::from_str(line)
        .map_err(|error| format!("bundled definition `{id}` is malformed: {error}"))?;
    if row.id != id {
        return Err(format!("bundled definition `{id}` has a mismatched id"));
    }
    Ok(Some(row.definition_yaml))
}

fn find_body_line<'a>(asset: &'a str, id: &str) -> Option<&'a str> {
    let prefix = format!("{{\"id\":{},", serde_json::to_string(id).ok()?);
    asset
        .lines()
        .map(str::trim_end)
        .find(|line| line.starts_with(&prefix))
}

fn parse_index(asset: &str) -> Result<Vec<IndexRow>, String> {
    let mut rows = Vec::new();
    let mut ids = BTreeSet::new();
    let mut labels = BTreeSet::new();
    for (offset, line) in asset.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let number = offset + 1;
        let row: IndexRow = serde_json::from_str(line)
            .map_err(|error| format!("definition index row {number} is invalid: {error}"))?;
        if row.schema_version != ASSET_SCHEMA_VERSION {
            return Err(format!(
                "definition index row {number} uses unsupported schema version {}",
                row.schema_version
            ));
        }
        if row.id.trim().is_empty() || row.id == CUSTOM_DEFINITION_ID {
            return Err(format!("definition index row {number} has an unusable id"));
        }
        if !ids.insert(row.id.clone()) {
            return Err(format!(
                "definition index row {number} repeats id `{}`",
                row.id
            ));
        }
        if row.display_name.trim().is_empty() {
            return Err(format!(
                "definition index row {number} has an empty display name"
            ));
        }
        // Two trackers the operator cannot tell apart in a 500-entry select is
        // a usability bug, so the generator has to have made labels unique.
        if !labels.insert(option_label(&row)) {
            return Err(format!(
                "definition index row {number} repeats label `{}`",
                option_label(&row)
            ));
        }
        if !row.base_url.starts_with("http://") && !row.base_url.starts_with("https://") {
            return Err(format!(
                "definition index row {number} has a non-HTTP base URL `{}`",
                row.base_url
            ));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err("bundled Cardigann definition index is empty".to_string());
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_index_and_bodies_describe_the_same_definitions() {
        let rows = index().expect("bundled Cardigann definition index must be valid");
        assert!(
            rows.len() > 500,
            "expected the full Prowlarr v11 corpus, got {}",
            rows.len()
        );
        let bodies = BODIES_ASSET
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        assert_eq!(rows.len(), bodies, "index and bodies must stay in lockstep");
        for row in &rows {
            let yaml = definition_yaml(&row.id)
                .expect("bundled body must parse")
                .unwrap_or_else(|| panic!("`{}` has no bundled body", row.id));
            assert!(!yaml.trim().is_empty(), "`{}` has an empty body", row.id);
        }
        assert!(
            definition_yaml("definitely-not-a-baked-tracker")
                .unwrap()
                .is_none()
        );
        assert!(definition_yaml(CUSTOM_DEFINITION_ID).unwrap().is_none());
    }

    /// The committed asset is only ever allowed to hold definitions that clear
    /// the same gate `xtask cardigann sync-definitions` applies, so a hand-edit
    /// that sneaks a broken definition in fails here rather than at search time.
    #[test]
    fn every_bundled_definition_clears_the_sync_gate() {
        let rows = index().expect("bundled Cardigann definition index must be valid");
        let mut failures = Vec::new();
        for row in &rows {
            let yaml = definition_yaml(&row.id).unwrap().expect("bundled body");
            match crate::gate::gate_definition(&yaml) {
                Ok(entry) => {
                    if entry.id != row.id {
                        failures.push(format!("{}: definition declares id `{}`", row.id, entry.id));
                    }
                    if entry.base_url != row.base_url {
                        failures.push(format!(
                            "{}: index base URL `{}` is not the definition's first link `{}`",
                            row.id, row.base_url, entry.base_url
                        ));
                    }
                }
                Err(error) => failures.push(format!("{}: {error}", row.id)),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn selector_offers_custom_first_and_prefills_only_the_base_url() {
        let rows = index().unwrap();
        let field = selector(&rows);
        assert_eq!(field.key, "definition");
        assert!(!field.required);
        assert_eq!(field.options.len(), rows.len() + 1);
        assert_eq!(field.options[0].value, CUSTOM_DEFINITION_ID);
        assert!(field.options[0].config_overrides.is_empty());
        for option in field.options.iter().skip(1) {
            assert_eq!(
                option.config_overrides.keys().collect::<Vec<_>>(),
                vec!["base_url"],
                "`{}` must prefill nothing but the base URL",
                option.value
            );
            assert!(!option.label.trim().is_empty());
        }
    }

    #[test]
    fn index_parsing_rejects_malformed_rows() {
        assert!(parse_index("").is_err());
        assert!(
            parse_index(
                r#"{"schema_version":2,"id":"a","display_name":"A","kind":"public","base_url":"https://a.example/"}"#
            )
            .is_err()
        );
        assert!(
            parse_index(
                r#"{"schema_version":1,"id":"custom","display_name":"A","kind":"public","base_url":"https://a.example/"}"#
            )
            .is_err()
        );
        assert!(
            parse_index(
                r#"{"schema_version":1,"id":"a","display_name":"A","kind":"public","base_url":"ftp://a.example/"}"#
            )
            .is_err()
        );
        let duplicated = concat!(
            r#"{"schema_version":1,"id":"a","display_name":"A","kind":"public","base_url":"https://a.example/"}"#,
            "\n",
            r#"{"schema_version":1,"id":"a","display_name":"B","kind":"public","base_url":"https://b.example/"}"#,
        );
        assert!(parse_index(duplicated).is_err());
        let repeated_label = concat!(
            r#"{"schema_version":1,"id":"a","display_name":"A","kind":"public","base_url":"https://a.example/"}"#,
            "\n",
            r#"{"schema_version":1,"id":"b","display_name":"A","kind":"public","base_url":"https://b.example/"}"#,
        );
        assert!(parse_index(repeated_label).is_err());
    }

    #[test]
    fn body_lookup_matches_whole_ids_only() {
        let asset = concat!(
            r#"{"id":"tracker","definition_yaml":"id: tracker\n"}"#,
            "\n",
            r#"{"id":"tracker-two","definition_yaml":"id: tracker-two\n"}"#,
            "\n",
        );
        assert_eq!(
            find_body_line(asset, "tracker"),
            Some(r#"{"id":"tracker","definition_yaml":"id: tracker\n"}"#)
        );
        assert_eq!(
            find_body_line(asset, "tracker-two"),
            Some(r#"{"id":"tracker-two","definition_yaml":"id: tracker-two\n"}"#)
        );
        assert_eq!(find_body_line(asset, "track"), None);
    }
}
