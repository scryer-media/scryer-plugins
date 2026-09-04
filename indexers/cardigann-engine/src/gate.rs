//! The admission gate for the baked definition corpus.
//!
//! One definition is admissible when it parses as Cardigann v11, validates
//! against the schema this engine actually supports, and drives the search flow
//! far enough to produce its first HTTP request. That is the same bar the v11
//! corpus test holds the upstream repository to, and the same bar
//! `xtask cardigann sync-definitions` applies before writing the committed
//! asset — one implementation, used by both, so the shipped corpus can never
//! contain something the corpus test would have rejected.
//!
//! This module is test-only on purpose. Nothing the component does at runtime
//! needs it, and compiling a corpus generator into the shipped Wasm would be
//! dead weight. `xtask cardigann sync-definitions` therefore reaches it by
//! running this crate's test binary with the `CARDIGANN_GATE_*` variables set,
//! which keeps the asset's reader (`crate::baked`) and its writer in one crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::baked::{ASSET_SCHEMA_VERSION, BodyRow, CUSTOM_DEFINITION_ID, IndexRow};
use crate::{definition, parse_definition, runtime};

const SOURCE_DIR_ENV: &str = "CARDIGANN_GATE_SOURCE_DIR";
const INDEX_OUT_ENV: &str = "CARDIGANN_GATE_INDEX_OUT";
const BODIES_OUT_ENV: &str = "CARDIGANN_GATE_BODIES_OUT";
const REPORT_OUT_ENV: &str = "CARDIGANN_GATE_REPORT_OUT";

/// What the gate learned about an admissible definition.
#[derive(Debug, Clone)]
pub struct GateEntry {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub base_url: String,
}

/// Run one definition through the gate.
pub fn gate_definition(source: &str) -> Result<GateEntry, String> {
    let definition = parse_definition(source)?;
    let base_url = definition
        .links
        .first()
        .cloned()
        .ok_or_else(|| "definition has no base URL".to_string())?;
    let entry = GateEntry {
        id: definition.id.clone(),
        display_name: definition.name.clone(),
        kind: definition.definition_type.clone(),
        base_url: base_url.clone(),
    };
    let compiled_ir = serde_json::to_string(&definition::CompiledIr {
        ir_version: definition::COMPILED_IR_VERSION,
        definition,
    })
    .map_err(|error| format!("could not encode IR: {error}"))?;
    match runtime::begin(
        compiled_ir,
        runtime::Operation::Search(Box::default()),
        BTreeMap::from([("base_url".to_string(), base_url)]),
    ) {
        Ok(runtime::Step::NeedHttp { .. }) => Ok(entry),
        Ok(step) => Err(format!("flow did not begin with HTTP: {step:?}")),
        Err(error) => Err(format!("flow start failed: {error}")),
    }
}

/// Every `*.yml` / `*.yaml` file in a Prowlarr definitions directory, sorted.
pub fn candidate_files(source_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = std::fs::read_dir(source_dir)
        .map_err(|error| format!("could not read {}: {error}", source_dir.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("could not read a corpus entry: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| {
            matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("yml" | "yaml")
            )
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

#[derive(Debug, Serialize)]
pub struct Excluded {
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub schema_version: u32,
    pub source_dir: String,
    pub candidates: usize,
    pub accepted: usize,
    pub excluded: Vec<Excluded>,
}

struct Admitted {
    entry: GateEntry,
    source: String,
}

/// Gate a whole source directory and render the two asset files plus a report.
fn sync(source_dir: &Path) -> Result<(String, String, SyncReport), String> {
    let candidates = candidate_files(source_dir)?;
    if candidates.is_empty() {
        return Err(format!(
            "{} contains no Cardigann definitions",
            source_dir.display()
        ));
    }

    let mut admitted: Vec<Admitted> = Vec::new();
    let mut excluded = Vec::new();
    let mut seen_ids: BTreeMap<String, String> = BTreeMap::new();
    for path in &candidates {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                excluded.push(Excluded {
                    file,
                    reason: format!("could not read: {error}"),
                });
                continue;
            }
        };
        match gate_definition(&source) {
            Ok(entry) => {
                // The id is the selector's stored value and the asset's lookup
                // key, so a collision has to be an exclusion rather than a
                // silent last-writer-wins.
                if entry.id == CUSTOM_DEFINITION_ID {
                    excluded.push(Excluded {
                        file,
                        reason: format!("id `{CUSTOM_DEFINITION_ID}` is reserved"),
                    });
                    continue;
                }
                if let Some(previous) = seen_ids.get(&entry.id) {
                    excluded.push(Excluded {
                        file,
                        reason: format!("id `{}` is already used by {previous}", entry.id),
                    });
                    continue;
                }
                seen_ids.insert(entry.id.clone(), file);
                admitted.push(Admitted { entry, source });
            }
            Err(reason) => excluded.push(Excluded { file, reason }),
        }
    }
    if admitted.is_empty() {
        return Err("no definition cleared the gate".to_string());
    }

    admitted.sort_by(|left, right| left.entry.id.cmp(&right.entry.id));
    let labels = unique_labels(&admitted);

    let mut index = String::new();
    let mut bodies = String::new();
    for (position, item) in admitted.iter().enumerate() {
        let (display_name, kind) = labels[position].clone();
        let row = IndexRow {
            schema_version: ASSET_SCHEMA_VERSION,
            id: item.entry.id.clone(),
            display_name,
            kind,
            base_url: item.entry.base_url.clone(),
        };
        index.push_str(
            &serde_json::to_string(&row)
                .map_err(|error| format!("could not encode index row: {error}"))?,
        );
        index.push('\n');
        let body = BodyRow {
            id: item.entry.id.clone(),
            definition_yaml: item.source.clone(),
        };
        bodies.push_str(
            &serde_json::to_string(&body)
                .map_err(|error| format!("could not encode body row: {error}"))?,
        );
        bodies.push('\n');
    }

    let report = SyncReport {
        schema_version: ASSET_SCHEMA_VERSION,
        source_dir: source_dir.display().to_string(),
        candidates: candidates.len(),
        accepted: admitted.len(),
        excluded,
    };
    Ok((index, bodies, report))
}

/// Give every option a label an operator can tell apart.
///
/// Trackers do reuse display names across definitions, and a select with two
/// identical rows is unusable, so a repeated `name (type)` pair falls back to
/// disambiguating on the definition id.
fn unique_labels(admitted: &[Admitted]) -> Vec<(String, String)> {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for item in admitted {
        *counts
            .entry((item.entry.display_name.clone(), item.entry.kind.clone()))
            .or_default() += 1;
    }
    admitted
        .iter()
        .map(|item| {
            let key = (item.entry.display_name.clone(), item.entry.kind.clone());
            let display_name = if counts.get(&key).copied().unwrap_or_default() > 1 {
                format!("{} [{}]", item.entry.display_name, item.entry.id)
            } else {
                item.entry.display_name.clone()
            };
            (display_name, item.entry.kind.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator half of `xtask cardigann sync-definitions`.
    ///
    /// It is inert unless the xtask points it at a Prowlarr checkout, and it
    /// only ever writes to the paths the xtask hands it — never into the source
    /// tree. The xtask moves the rendered files into place itself.
    #[test]
    fn writes_definition_gate_report() {
        let Some(source_dir) = std::env::var_os(SOURCE_DIR_ENV) else {
            return;
        };
        let source_dir = PathBuf::from(source_dir);
        let (index, bodies, report) = sync(&source_dir).expect("gate the definition corpus");
        for (variable, contents) in [
            (INDEX_OUT_ENV, index),
            (BODIES_OUT_ENV, bodies),
            (
                REPORT_OUT_ENV,
                serde_json::to_string_pretty(&report).expect("encode the sync report"),
            ),
        ] {
            let path = std::env::var_os(variable)
                .unwrap_or_else(|| panic!("{variable} must be set alongside {SOURCE_DIR_ENV}"));
            std::fs::write(&path, contents)
                .unwrap_or_else(|error| panic!("write {variable}: {error}"));
        }
        eprintln!(
            "Cardigann definition gate: candidates={}, accepted={}, excluded={}",
            report.candidates,
            report.accepted,
            report.excluded.len()
        );
    }

    #[test]
    fn gate_rejects_definitions_that_cannot_reach_a_request() {
        assert!(gate_definition("not: a definition").is_err());
        let entry = gate_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps: {}
search:
  paths: [{ path: search }]
"#,
        )
        .expect("a minimal definition clears the gate");
        assert_eq!(entry.id, "fixture");
        assert_eq!(entry.display_name, "Fixture");
        assert_eq!(entry.kind, "public");
        assert_eq!(entry.base_url, "https://tracker.example/");
    }
}
