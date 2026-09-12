use super::*;

fn fixture(dir: &Path) -> CatalogV3 {
    let artifact_path = dir.join("pack.hash.min.json.zst");
    fs::write(&artifact_path, b"compressed pack fixture").unwrap();
    fs::write(
        dir.join("pack.hash.min.json.zst.bundle.zst"),
        b"bundle fixture",
    )
    .unwrap();
    let artifact = CatalogV3Artifact {
        url: "https://example.test/packs/v1.0.0/pack.hash.min.json.zst".into(),
        signature_url: "https://example.test/packs/v1.0.0/pack.hash.min.json.zst.bundle.zst".into(),
        mirror_urls: Vec::new(),
        signature_mirror_urls: Vec::new(),
        digests: file_digests(&artifact_path).unwrap(),
    };
    write_manifest(dir, std::slice::from_ref(&artifact));
    CatalogV3 {
        schema_version: CATALOG_V3_SCHEMA.into(),
        catalog_version: 2,
        plugins: Vec::new(),
        community_sources: Vec::new(),
        rule_packs: vec![CatalogV3RulePackEntry {
            id: "pack".into(),
            name: "Pack".into(),
            description: "Fixture".into(),
            author: "community".into(),
            releases: vec![CatalogV3RulePackRelease {
                version: "1.0.0".into(),
                min_scryer_version: Some("0.20.0".into()),
                rule_pack_digests: Vec::new(),
                rule_pack_bytes: None,
                customizable: true,
                artifacts: vec![artifact],
            }],
        }],
    }
}

fn write_manifest(dir: &Path, artifacts: &[CatalogV3Artifact]) {
    fs::write(
        dir.join(PREPARED_RULE_PACK_ARTIFACTS),
        serde_json::to_vec(artifacts).unwrap(),
    )
    .unwrap();
}

#[test]
fn preflight_checks_only_selected_artifacts_and_preserves_remote_history() {
    let temp = tempfile::tempdir().unwrap();
    let mut catalog = fixture(temp.path());
    let mut historical = catalog.rule_packs[0].releases[0].clone();
    historical.version = "0.9.0".into();
    historical.artifacts[0].url = "https://example.test/old.min.json.zst".into();
    catalog.rule_packs[0].releases.push(historical);
    assert_eq!(
        prepared_rule_pack_artifacts_for_upload(temp.path(), &catalog)
            .unwrap()
            .len(),
        1
    );
    write_manifest(temp.path(), &[]);
    fs::remove_file(temp.path().join("pack.hash.min.json.zst")).unwrap();
    assert!(
        prepared_rule_pack_artifacts_for_upload(temp.path(), &catalog)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn preflight_rejects_missing_new_blob_or_signature_before_upload() {
    for filename in [
        "pack.hash.min.json.zst",
        "pack.hash.min.json.zst.bundle.zst",
        PREPARED_RULE_PACK_ARTIFACTS,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let catalog = fixture(temp.path());
        fs::remove_file(temp.path().join(filename)).unwrap();
        assert!(
            prepared_rule_pack_artifacts_for_upload(temp.path(), &catalog).is_err(),
            "accepted missing {filename}"
        );
    }
}

#[test]
fn preflight_rejects_changed_bytes_and_unlisted_or_duplicate_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = fixture(temp.path());
    fs::write(temp.path().join("pack.hash.min.json.zst"), b"changed").unwrap();
    assert!(
        prepared_rule_pack_artifacts_for_upload(temp.path(), &catalog)
            .unwrap_err()
            .to_string()
            .contains("digest mismatch")
    );

    let catalog = fixture(temp.path());
    let artifact = catalog.rule_packs[0].releases[0].artifacts[0].clone();
    write_manifest(temp.path(), &[artifact.clone(), artifact.clone()]);
    assert!(
        prepared_rule_pack_artifacts_for_upload(temp.path(), &catalog)
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );
    let mut unlisted = artifact;
    unlisted.url = "https://example.test/unlisted.min.json.zst".into();
    write_manifest(temp.path(), &[unlisted]);
    assert!(
        prepared_rule_pack_artifacts_for_upload(temp.path(), &catalog)
            .unwrap_err()
            .to_string()
            .contains("absent from catalog")
    );
}
