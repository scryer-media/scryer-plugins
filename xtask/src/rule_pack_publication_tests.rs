use super::*;
use std::fs;
use std::path::{Path, PathBuf};

const DIGEST: &str = "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn temp_context() -> (tempfile::TempDir, TaskContext) {
    let directory = tempfile::tempdir().expect("create fixture repository");
    let context = TaskContext {
        repo_root: directory.path().to_path_buf(),
    };
    (directory, context)
}

fn write_file(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().expect("fixture file has a parent"))
        .expect("create fixture directory");
    fs::write(path, contents).expect("write fixture file");
}

fn rule_pack_artifact(id: &str, version: &str) -> CatalogV3Artifact {
    let name = format!("{id}.min.json.zst");
    CatalogV3Artifact {
        url: format!("https://cdn.example.invalid/rule-packs/{version}/{name}"),
        mirror_urls: Vec::new(),
        signature_url: format!(
            "https://cdn.example.invalid/rule-packs/{version}/{name}.bundle.zst"
        ),
        signature_mirror_urls: Vec::new(),
        digests: vec![DIGEST.to_string()],
    }
}

fn historical_rule_pack(id: &str, version: &str) -> CatalogV3RulePackEntry {
    CatalogV3RulePackEntry {
        id: id.to_string(),
        name: format!("{id} name"),
        description: format!("{id} description"),
        author: "fixture".to_string(),
        releases: vec![CatalogV3RulePackRelease {
            version: version.to_string(),
            min_scryer_version: Some("0.20.0".to_string()),
            rule_pack_digests: vec![DIGEST.to_string()],
            rule_pack_bytes: Some(12),
            customizable: true,
            artifacts: vec![rule_pack_artifact(id, version)],
        }],
    }
}

fn historical_plugin() -> CatalogV3PluginEntry {
    let artifact_name = "history.wasm.zst";
    CatalogV3PluginEntry {
        id: "historical-plugin".to_string(),
        name: "Historical plugin".to_string(),
        description: "Preserved fixture plugin history".to_string(),
        plugin_type: "indexer".to_string(),
        provider_type: "indexer".to_string(),
        publisher: "scryer".to_string(),
        support_tier: "official".to_string(),
        status: PluginCatalogStatus::Active,
        docs_url: "https://example.invalid/docs".to_string(),
        source_repo: "https://github.com/scryer-media/scryer-plugins".to_string(),
        required_signer: RequiredSignerV2 {
            github_repository: OFFICIAL_GITHUB_REPO.to_string(),
            github_workflow: Some("release.yml".to_string()),
        },
        releases: vec![CatalogV3Release {
            version: "0.4.0".to_string(),
            sdk_constraint: ">=1.0.0".to_string(),
            min_scryer_version: None,
            max_scryer_version: None,
            artifacts: vec![CatalogV3PluginArtifact {
                runtime: TARGET_WASIP1.to_string(),
                required_features: Vec::new(),
                wasm_digests: vec![DIGEST.to_string()],
                bytes: 12,
                url: format!(
                    "https://cdn.example.invalid/plugins-v3/historical-plugin/v0.4.0/{artifact_name}"
                ),
                mirror_urls: Vec::new(),
                signature_url: format!(
                    "https://cdn.example.invalid/plugins-v3/historical-plugin/v0.4.0/{artifact_name}.bundle.zst"
                ),
                signature_mirror_urls: Vec::new(),
                digests: vec![DIGEST.to_string()],
            }],
        }],
    }
}

fn write_rule_pack_fixture(ctx: &TaskContext) {
    write_file(
        &ctx.path(RULE_PACK_SOURCE_MANIFEST),
        r#"{
  "rule_packs": [
    {
      "id": "selected-pack",
      "asset": "selected-pack.json",
      "distribution_base_url": "https://cdn.example.invalid/rule-packs",
      "min_scryer_version": "0.20.0"
    }
  ]
}
"#,
    );
    write_file(
        &ctx.repo_root.join("rule_packs/selected-pack.json"),
        r#"{
  "schema_version": 1,
  "id": "selected-pack",
  "name": "Selected pack",
  "description": "A tiny selected fixture pack.",
  "author": "fixture",
  "version": "1.1.0",
  "rules": [{"id": "fixture"}]
}
"#,
    );
    write_file(
        &ctx.path(CATALOG_V3_RELEASE_CONSTRAINTS),
        r#"{"release_constraints": []}"#,
    );
}

fn write_existing_catalog(ctx: &TaskContext) -> PathBuf {
    let existing = CatalogV3 {
        schema_version: CATALOG_V3_SCHEMA.to_string(),
        catalog_version: 7,
        plugins: vec![historical_plugin()],
        community_sources: Vec::new(),
        rule_packs: vec![
            historical_rule_pack("selected-pack", "1.0.0"),
            historical_rule_pack("unselected-pack", "0.5.0"),
        ],
    };
    validate_catalog_v3(&existing).expect("fixture baseline catalog is valid");
    let path = ctx.path("baseline/catalog-v3.json");
    write_file(
        &path,
        serde_json::to_vec_pretty(&existing).expect("serialize baseline catalog"),
    );
    path
}

fn pack_only_args(out: PathBuf, existing_catalog: Option<PathBuf>) -> CatalogPrepareV3Args {
    CatalogPrepareV3Args {
        out: Some(out),
        plugin_ids: Vec::new(),
        rule_pack_ids: vec!["selected-pack".to_string()],
        existing_catalog,
        prepared_plugin_root: None,
        allow_release_removals: Vec::new(),
        allow_stratum_drops: Vec::new(),
        allow_selected_rebuild: false,
    }
}

#[test]
fn pack_only_prepare_merges_baseline_and_stages_only_selected_new_rule_pack() {
    let (_directory, ctx) = temp_context();
    write_rule_pack_fixture(&ctx);
    let baseline = write_existing_catalog(&ctx);
    let out = ctx.path("out");

    run_catalog_prepare_v3(&ctx, pack_only_args(out.clone(), Some(baseline)))
        .expect("pack-only prepare should not require plugin sources or an SDK manifest");

    let catalog = read_catalog_v3_from_path(&ctx, &out.join(CATALOG_V3_SNIPPET_JSON))
        .expect("read prepared catalog");
    assert_eq!(catalog.catalog_version, 8);
    assert_eq!(catalog.plugins.len(), 1);
    assert_eq!(catalog.plugins[0].id, "historical-plugin");
    assert_eq!(catalog.plugins[0].releases[0].version, "0.4.0");

    let selected = catalog
        .rule_packs
        .iter()
        .find(|pack| pack.id == "selected-pack")
        .expect("selected pack is present");
    assert_eq!(
        selected
            .releases
            .iter()
            .map(|release| release.version.as_str())
            .collect::<Vec<_>>(),
        vec!["1.1.0", "1.0.0"]
    );
    let unselected = catalog
        .rule_packs
        .iter()
        .find(|pack| pack.id == "unselected-pack")
        .expect("unselected historical pack is preserved");
    assert_eq!(unselected.releases[0].version, "0.5.0");

    let staged: Vec<CatalogV3Artifact> = serde_json::from_slice(
        &fs::read(out.join(PREPARED_RULE_PACK_ARTIFACTS))
            .expect("read staged selected rule-pack artifacts"),
    )
    .expect("parse staged selected rule-pack artifacts");
    assert_eq!(staged.len(), 2);
    assert!(staged.iter().all(|artifact| {
        artifact.url.contains("selected-pack") && artifact.url.contains("/v1.1.0/")
    }));
    assert!(
        !out.join(rule_pack_minified_json_file_name("unselected-pack"))
            .exists(),
        "unselected pack must not be staged"
    );
    assert!(
        !ctx.path("Cargo.toml").exists(),
        "fixture intentionally has no SDK or plugin Cargo manifest"
    );
}

#[test]
fn pack_only_prepare_requires_baseline_before_writing() {
    let (_directory, ctx) = temp_context();
    write_rule_pack_fixture(&ctx);
    let out = ctx.path("out");

    let error = run_catalog_prepare_v3(&ctx, pack_only_args(out.clone(), None))
        .expect_err("a selected pack update without a baseline must fail");

    assert!(error.to_string().contains("requires --existing-catalog"));
    assert!(
        !out.exists(),
        "baseline failure must happen before writing rule-pack or catalog artifacts"
    );
}

#[test]
fn rule_pack_customizable_defaults_to_true_and_is_omitted_from_catalog_output() {
    let (_directory, ctx) = temp_context();
    write_rule_pack_fixture(&ctx);

    let manifest = load_rule_pack_manifest(&ctx.path("rule_packs/selected-pack.json"))
        .expect("load legacy rule-pack manifest");
    assert!(manifest.customizable);

    let release = CatalogV3RulePackRelease {
        version: "1.0.0".to_string(),
        min_scryer_version: None,
        rule_pack_digests: vec![DIGEST.to_string()],
        rule_pack_bytes: Some(1),
        customizable: true,
        artifacts: vec![rule_pack_artifact("selected-pack", "1.0.0")],
    };
    let serialized = serde_json::to_value(release).expect("serialize catalog release");
    assert!(serialized.get("customizable").is_none());
}

#[test]
fn rule_pack_customizable_false_propagates_to_catalog_release() {
    let (_directory, ctx) = temp_context();
    write_rule_pack_fixture(&ctx);
    write_file(
        &ctx.path("rule_packs/selected-pack.json"),
        r#"{
  "schema_version": 1,
  "id": "selected-pack",
  "name": "Selected pack",
  "description": "A tiny selected fixture pack.",
  "author": "fixture",
  "version": "1.1.0",
  "customizable": false,
  "rules": [{"id": "fixture"}]
}
"#,
    );
    let baseline = write_existing_catalog(&ctx);
    let out = ctx.path("out");

    run_catalog_prepare_v3(&ctx, pack_only_args(out.clone(), Some(baseline)))
        .expect("prepare catalog with non-customizable pack");

    let catalog = read_catalog_v3_from_path(&ctx, &out.join(CATALOG_V3_SNIPPET_JSON))
        .expect("read prepared catalog");
    let release = catalog
        .rule_packs
        .iter()
        .find(|pack| pack.id == "selected-pack")
        .expect("selected pack")
        .releases
        .iter()
        .find(|release| release.version == "1.1.0")
        .expect("selected pack release");
    assert!(!release.customizable);
}

#[test]
fn rule_pack_customizable_rejects_non_boolean_values() {
    let (_directory, ctx) = temp_context();
    write_file(
        &ctx.path("rule_packs/invalid.json"),
        r#"{
  "schema_version": 1,
  "id": "invalid",
  "name": "Invalid pack",
  "description": "Fixture",
  "author": "fixture",
  "version": "1.0.0",
  "customizable": "false",
  "rules": [{"id": "fixture"}]
}
"#,
    );

    let error = load_rule_pack_manifest(&ctx.path("rule_packs/invalid.json"))
        .expect_err("string customizable value must be rejected");
    assert!(format!("{error:#}").contains("invalid type"), "{error:#}");
}
