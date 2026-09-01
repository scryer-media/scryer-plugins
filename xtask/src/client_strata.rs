//! Client strata: who is out there, what they can parse, and what they get.
//!
//! A *stratum* is a band of shipped Scryer releases that share a catalog
//! tolerance. Shipped binaries are immutable, so a stratum's tolerance is a
//! historical fact, not a policy — it is read off the catalog client at the
//! release branches named on each row below.
//!
//! The render produces one **projection** of the master catalog per stratum,
//! filtered to what that stratum can actually parse and run, and places it at
//! the redirect position that stratum's client reads. Two guards then run
//! against every projection before anything is signed:
//!
//! * a *parse-back* guard that re-validates the projection with a pinned copy
//!   of that stratum's shipped validator (`shipped_parsers` below), and
//! * a *no-strand* guard that fails the render when a stratum would lose a
//!   plugin it can currently install.
//!
//! # Why this exists
//!
//! On 2026-08-28 the indexer family was published as `wasm32-wasip2` artifacts.
//! Every Scryer at or below 0.18.21 validates artifact `runtime` against a
//! hard-coded `wasm32-wasip1` and returns `Err` for the **whole document** on a
//! mismatch, so one artifact row took the entire plugin catalog offline for
//! every one of those installs: refresh failed, install and upgrade were
//! blocked, and a fresh install saw no plugins at all. The `min_scryer_version`
//! guard that was supposed to protect them runs after parsing and never got a
//! turn. The cut was rolled back the same hour
//! (`38a8da6 fix(ci): restore plugin release compatibility`).
//!
//! The wasip2 indexer cut later shipped anyway: the live catalog (checked at
//! `catalog_version` 48, 2026-09-01) carries wasip2 rows for the indexer
//! families on **both** redirect rungs, gated `min_scryer_version: "0.18.22"`.
//! Scryer 0.19.0+ parses and runs them (`catalog_v3_runtime_is_supported`
//! accepts `wasip1|wasip2` from `scryer-v0.19.0` on, and the wasmtime component
//! host loads `scryer:indexer` worlds 1.0/1.1). Every 0.18.x install has been
//! frozen on its cached catalog since those rows appeared — that trade was
//! already made in production, and this module does not re-litigate it for the
//! `.last()` rung, whose live readers are 0.19.0–0.19.6.
//!
//! What the strata preserve, per band:
//! * 0.19.0–0.19.6 keep exactly what they get today — wasip1 plus the wasip2
//!   rows they can actually execute (indexer components). No regression.
//! * ≤ 0.18.11 get their `.first()` rung back as a pure-wasip1 projection —
//!   the live rung is currently a document they reject, so this is a revival,
//!   not a constraint.
//! * 0.18.12–0.18.21 stay as production left them: their rung is shared with
//!   0.19.x and positional indexing offers no way to serve the two bands
//!   different documents. They remain frozen on cache.
//! * No future token — a wasip3 runtime, a new feature, a new field — can
//!   reach any shipped rung; only the modern redirect carries those.
//!
//! Nothing about the shipped validators is fixable in those binaries. It is
//! only fixable here, by never handing a band a document it cannot read.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use semver::{Version, VersionReq};

use crate::{
    CatalogV3, CatalogV3PluginArtifact, CatalogV3PluginEntry, CatalogV3Release, WasmRequiredFeature,
};

/// Where in the published redirect ladder a stratum's client looks.
///
/// `CatalogV3Redirect.artifacts` is an ordered list and shipped clients pick a
/// fixed rung by index — which is the whole problem: there are exactly two
/// addressable positions and no way to add a third for an existing binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RedirectSlot {
    /// `redirect.artifacts.first()` — Scryer ≤ 0.18.11.
    LegacyFirst,
    /// `redirect.artifacts.last()` — Scryer 0.18.12 … 0.19.6.
    LegacyLast,
    /// The capability-aware redirect published at its own URL, read only by
    /// clients that walk the ladder back instead of indexing into it.
    Modern,
}

impl RedirectSlot {
    pub fn is_legacy(self) -> bool {
        matches!(self, Self::LegacyFirst | Self::LegacyLast)
    }
}

/// What one band of shipped Scryers can parse and run.
#[derive(Clone, Copy, Debug)]
pub struct ClientStratum {
    pub id: &'static str,
    pub label: &'static str,
    /// Semver range of Scryer releases in this band, for guard messages.
    pub scryer_range: &'static str,
    /// Artifact `runtime` values this band's *validator* accepts. Anything else
    /// makes the whole document unparseable for it.
    pub targets: &'static [&'static str],
    /// `plugin_type` values whose `wasm32-wasip2` artifacts this band can
    /// actually *execute*. Parse tolerance and runnability are different
    /// facts: 0.19.x parses a wasip2 row for any family but its component host
    /// only speaks the `scryer:indexer` worlds, so a wasip2 subtitle artifact
    /// would be downloaded and then fail to load. The projection keeps wasip2
    /// rows only for the types listed here.
    pub component_plugin_types: &'static [&'static str],
    /// `required_features` tokens this band accepts. Same all-or-nothing rule.
    pub features: &'static [WasmRequiredFeature],
    /// Whether the band's `CatalogV3PluginRelease` struct has a
    /// `max_scryer_version` field. It does not below 0.18.12, and every wire
    /// struct there carries `deny_unknown_fields`.
    pub allows_max_scryer_version: bool,
    pub slot: RedirectSlot,
}

pub const TARGET_WASIP1: &str = "wasm32-wasip1";
pub const TARGET_WASIP2: &str = "wasm32-wasip2";

const ALL_FEATURES: &[WasmRequiredFeature] = &[
    WasmRequiredFeature::Simd128,
    WasmRequiredFeature::RelaxedSimd,
];

/// The strata this repository publishes for.
///
/// Ordered oldest-tolerating first. Removing a row is how a band is
/// desupported, and the no-strand guard will then say exactly what that band
/// loses.
///
/// Evidence for every row, read off `scryer-media/scryer` tags on 2026-09-01:
/// * `scryer-v0.18.11:crates/scryer-application/src/plugins/catalog.rs`
///   — `CatalogV3PluginRelease` has no `max_scryer_version`; every struct is
///   `deny_unknown_fields`; `runtime != "wasm32-wasip1"` returns `Err`;
///   `catalog_fetch.rs` takes `redirect.artifacts.first()`.
/// * `scryer-v0.18.12` … `scryer-v0.18.21` — same runtime check,
///   `max_scryer_version` present, `redirect.artifacts.last()`.
/// * `scryer-v0.19.0` … `scryer-v0.19.6` — `wasip1` **or** `wasip2` accepted
///   (`catalog_v3_runtime_is_supported`), still `redirect.artifacts.last()`,
///   still `deny_unknown_fields`, feature allow-list still
///   `simd128|relaxed-simd`; the component host executes wasip2 for the
///   indexer families only (`scryer:indexer/indexer-plugin@1.0.0`/`@1.1.0` in
///   `wasmtime_host/component_host.rs`). There was no 0.18.22 release — the
///   tag sequence jumps from 0.18.21 to 0.19.0.
///
/// The 0.18.12–0.18.21 band and the 0.19.x band read the **same** rung, and
/// positional indexing offers no way to hand them different documents. The
/// rung serves the 0.19.x tolerance because that is what production already
/// does (the live catalog carries wasip2 indexer rows) and 0.19.x is the band
/// still being served; 0.18.12–0.18.21 were frozen when those rows first
/// published. Serving anything *newer* than the 0.19.x tolerance through this
/// rung would freeze 0.19.x the same way, which is why the modern stratum gets
/// its own URL.
pub const CLIENT_STRATA: &[ClientStratum] = &[
    ClientStratum {
        id: "legacy-no-release-ceiling",
        label: "Scryer 0.18.11 and older",
        scryer_range: "<0.18.12",
        targets: &[TARGET_WASIP1],
        component_plugin_types: &[],
        features: ALL_FEATURES,
        allows_max_scryer_version: false,
        slot: RedirectSlot::LegacyFirst,
    },
    ClientStratum {
        id: "shipped-preview2",
        // 0.18.12–0.18.21 read this rung too, but cannot parse the wasip2 rows
        // production already serves on it; the label names the band the rung
        // still serves.
        label: "Scryer 0.19.0 through 0.19.6",
        scryer_range: ">=0.19.0, <0.19.7",
        targets: &[TARGET_WASIP1, TARGET_WASIP2],
        component_plugin_types: INDEXER_PLUGIN_TYPES,
        features: ALL_FEATURES,
        allows_max_scryer_version: true,
        slot: RedirectSlot::LegacyLast,
    },
    ClientStratum {
        id: "modern",
        label: "Scryer 0.19.7 and newer",
        // Capability-aware clients skip artifacts they cannot run instead of
        // rejecting the document, so this stratum takes the master catalog
        // unfiltered and needs no target or feature list of its own. The lists
        // below are the render's own allow-list, not a client limit.
        scryer_range: ">=0.19.7",
        targets: &[TARGET_WASIP1, TARGET_WASIP2],
        component_plugin_types: &[],
        features: ALL_FEATURES,
        allows_max_scryer_version: true,
        slot: RedirectSlot::Modern,
    },
];

/// The `plugin_type` labels the indexer family publishes under — the only
/// families whose wasip2 components a 0.19.x host can execute, and exactly the
/// set carrying wasip2 rows in the live catalog today.
const INDEXER_PLUGIN_TYPES: &[&str] = &["indexer", "torrent_indexer", "usenet_indexer"];

pub fn stratum_by_id(id: &str) -> Option<&'static ClientStratum> {
    CLIENT_STRATA.iter().find(|stratum| stratum.id == id)
}

impl ClientStratum {
    /// The master catalog is handed to this stratum unchanged.
    pub fn takes_master_catalog(&self) -> bool {
        self.slot == RedirectSlot::Modern
    }

    /// Whether this band's client can parse *and run* `artifact` when it
    /// belongs to a plugin of `plugin_type`.
    ///
    /// A wasip2 row must clear both bars: the band's validator must accept the
    /// runtime string, and the band's component host must speak that family's
    /// world. A row that parses but cannot load would be selected, downloaded,
    /// and then break that plugin on every install in the band.
    fn accepts_artifact(&self, plugin_type: &str, artifact: &CatalogV3PluginArtifact) -> bool {
        let runtime = artifact.runtime.trim();
        if !self.targets.contains(&runtime) {
            return false;
        }
        if runtime == TARGET_WASIP2 && !self.component_plugin_types.contains(&plugin_type) {
            return false;
        }
        artifact
            .required_features
            .iter()
            .all(|feature| self.features.contains(feature))
    }
}

/// One rendered projection plus the stratum it belongs to.
#[derive(Clone, Debug)]
pub struct StratumProjection {
    pub stratum: &'static ClientStratum,
    pub catalog: CatalogV3,
}

/// Filter `master` down to what `stratum` can parse and run.
///
/// Artifacts the stratum cannot read are dropped; a release left with no
/// artifacts is dropped; a plugin left with no releases is dropped. Fields the
/// stratum's struct does not have are stripped.
pub fn project_catalog_for_stratum(master: &CatalogV3, stratum: &ClientStratum) -> CatalogV3 {
    let mut projection = master.clone();
    if stratum.takes_master_catalog() {
        return projection;
    }

    let mut plugins = Vec::new();
    for plugin in &projection.plugins {
        let mut releases = Vec::new();
        for release in &plugin.releases {
            let artifacts = release
                .artifacts
                .iter()
                .filter(|artifact| stratum.accepts_artifact(&plugin.plugin_type, artifact))
                .cloned()
                .collect::<Vec<_>>();
            if artifacts.is_empty() {
                continue;
            }
            let mut release = CatalogV3Release {
                artifacts,
                ..release.clone()
            };
            if !stratum.allows_max_scryer_version {
                release.max_scryer_version = None;
            }
            releases.push(release);
        }
        if releases.is_empty() {
            continue;
        }
        plugins.push(CatalogV3PluginEntry {
            releases,
            ..plugin.clone()
        });
    }
    projection.plugins = plugins;
    projection
}

/// Plugin ids a stratum's client would be able to install from `catalog`.
///
/// This mirrors the shipped selection rule: a plugin is installable when at
/// least one of its releases carries an artifact the stratum accepts. Scryer
/// version bounds are deliberately *not* applied — a release capped below the
/// band still serves the band's older members, and the guard's job is to catch
/// a plugin disappearing outright, not to model every member's exact pick.
pub fn installable_plugin_ids(catalog: &CatalogV3, stratum: &ClientStratum) -> BTreeSet<String> {
    catalog
        .plugins
        .iter()
        .filter(|plugin| {
            plugin.releases.iter().any(|release| {
                release
                    .artifacts
                    .iter()
                    .any(|artifact| stratum.accepts_artifact(&plugin.plugin_type, artifact))
            })
        })
        .map(|plugin| plugin.id.clone())
        .collect()
}

/// A plugin a stratum is about to lose, named the way the guard reports it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StratumDrop {
    pub plugin_id: String,
    pub stratum_id: String,
}

impl std::fmt::Display for StratumDrop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}@{}", self.plugin_id, self.stratum_id)
    }
}

pub fn parse_stratum_drop(value: &str) -> Result<StratumDrop> {
    let (plugin_id, stratum_id) = value.trim().rsplit_once('@').ok_or_else(|| {
        anyhow::anyhow!("--allow-stratum-drop expects PLUGIN_ID@STRATUM_ID, got '{value}'")
    })?;
    let plugin_id = plugin_id.trim();
    let stratum_id = stratum_id.trim();
    if plugin_id.is_empty() {
        bail!("--allow-stratum-drop is missing a plugin id in '{value}'");
    }
    if stratum_by_id(stratum_id).is_none() {
        bail!(
            "--allow-stratum-drop names unknown stratum '{stratum_id}'; known strata: {}",
            CLIENT_STRATA
                .iter()
                .map(|stratum| stratum.id)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(StratumDrop {
        plugin_id: plugin_id.to_string(),
        stratum_id: stratum_id.to_string(),
    })
}

pub fn parse_stratum_drops(values: &[String]) -> Result<BTreeSet<StratumDrop>> {
    values
        .iter()
        .map(|value| parse_stratum_drop(value))
        .collect()
}

/// Render every stratum's projection and refuse to publish a stranding cut.
///
/// `existing` is the previously published catalog, when the render has one; the
/// no-strand guard compares against what each stratum can install *today*.
pub fn build_publication_strata(
    master: &CatalogV3,
    existing: Option<&CatalogV3>,
    allowed_drops: &BTreeSet<StratumDrop>,
) -> Result<Vec<StratumProjection>> {
    let mut projections = Vec::new();
    let mut occupied_slots: BTreeMap<RedirectSlot, &'static str> = BTreeMap::new();
    let mut strandings = Vec::new();

    for stratum in CLIENT_STRATA {
        if let Some(previous) = occupied_slots.insert(stratum.slot, stratum.id) {
            bail!(
                "client strata '{previous}' and '{}' both claim redirect slot {:?}; shipped \
                 clients index that slot by position, so the two bands would have to share one \
                 document — give one of them its own redirect URL instead",
                stratum.id,
                stratum.slot
            );
        }

        let catalog = project_catalog_for_stratum(master, stratum);

        if !master.plugins.is_empty() && catalog.plugins.is_empty() {
            bail!(
                "catalog projection for stratum '{}' ({}) is empty while the master catalog has \
                 {} plugin(s); publishing it would take the plugin catalog away from every \
                 install in that band",
                stratum.id,
                stratum.label,
                master.plugins.len()
            );
        }

        shipped_parsers::validate_as_stratum(stratum, &catalog)?;

        // Stranding is a projection effect: a plugin that is still in the
        // master catalog but disappears from what this band can read. A stratum
        // that takes the master catalog unfiltered cannot be stranded — if a
        // plugin is gone from there it is a deliberate removal, which
        // `validate_catalog_v3_preserves_existing_releases` already gates with
        // `--allow-release-removal`.
        if let Some(existing) = existing.filter(|_| !stratum.takes_master_catalog()) {
            let had = installable_plugin_ids(existing, stratum);
            let has = installable_plugin_ids(&catalog, stratum);
            for plugin_id in had.difference(&has) {
                let drop = StratumDrop {
                    plugin_id: plugin_id.clone(),
                    stratum_id: stratum.id.to_string(),
                };
                if !allowed_drops.contains(&drop) {
                    strandings.push(drop);
                }
            }
        }

        projections.push(StratumProjection { stratum, catalog });
    }

    if !strandings.is_empty() {
        strandings.sort();
        let detail = strandings
            .iter()
            .map(|drop| match stratum_by_id(&drop.stratum_id) {
                Some(stratum) => format!(
                    "{drop} (loses '{}', Scryer {})",
                    stratum.label, stratum.scryer_range
                ),
                None => drop.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "this catalog would strand shipped clients: {detail}; keep a release those clients \
             can run, or pass --allow-stratum-drop PLUGIN_ID@STRATUM_ID to publish the removal \
             deliberately"
        );
    }

    if !projections
        .iter()
        .any(|projection| projection.stratum.slot == RedirectSlot::LegacyFirst)
        || !projections
            .iter()
            .any(|projection| projection.stratum.slot == RedirectSlot::LegacyLast)
    {
        bail!(
            "the legacy redirect ladder needs both a first and a last rung; shipped clients index \
             it by position and a missing rung silently re-points a whole band"
        );
    }

    Ok(projections)
}

/// Pinned re-implementations of the catalog validators that shipped.
///
/// These are copies, on purpose. Validating a projection with the current tree's
/// validator proves nothing: the question is whether a binary released in
/// August 2026 can read the document, and that binary's rules are frozen. Each
/// function names the release branch it was transcribed from; when a stratum is
/// desupported, delete its row from `CLIENT_STRATA` and its function here.
mod shipped_parsers {
    use super::*;

    /// Transcribed from `scryer-media/scryer`
    /// `origin/release-0.18.21:crates/scryer-application/src/plugins/catalog.rs`
    /// (`validate_plugin_release_set`, `validate_catalog_v3`), which is
    /// byte-identical in the relevant parts back to `origin/release-0.18.0`.
    /// `scryer-v0.19.0` through `scryer-v0.19.6` differ only in also accepting
    /// `wasm32-wasip2` (`catalog_v3_runtime_is_supported`, re-verified against
    /// the tags 2026-09-01), which is expressed by the stratum's `targets`
    /// list. Note this checks parse *tolerance* only: a 0.19.x client parses a
    /// wasip2 row for any plugin family — whether it can run one is
    /// `component_plugin_types`, enforced by the projection, not here.
    pub fn validate_as_stratum(stratum: &ClientStratum, catalog: &CatalogV3) -> Result<()> {
        if stratum.takes_master_catalog() {
            return Ok(());
        }
        for plugin in &catalog.plugins {
            let mut versions = BTreeSet::new();
            for release in &plugin.releases {
                Version::parse(&release.version).map_err(|error| {
                    anyhow::anyhow!(
                        "stratum '{}': plugin '{}' release version '{}' does not parse: {error}",
                        stratum.id,
                        plugin.id,
                        release.version
                    )
                })?;
                VersionReq::parse(&release.sdk_constraint).map_err(|error| {
                    anyhow::anyhow!(
                        "stratum '{}': plugin '{}' release '{}' sdk_constraint does not parse: \
                         {error}",
                        stratum.id,
                        plugin.id,
                        release.version
                    )
                })?;
                if !versions.insert(release.version.clone()) {
                    bail!(
                        "stratum '{}': plugin '{}' has duplicate release '{}'",
                        stratum.id,
                        plugin.id,
                        release.version
                    );
                }
                // `deny_unknown_fields` on `CatalogV3PluginRelease` below
                // 0.18.12: the field is not merely ignored, its presence is a
                // parse error for the whole document.
                if !stratum.allows_max_scryer_version && release.max_scryer_version.is_some() {
                    bail!(
                        "stratum '{}' ({}) cannot parse max_scryer_version, and plugin '{}' \
                         release '{}' still carries it",
                        stratum.id,
                        stratum.label,
                        plugin.id,
                        release.version
                    );
                }
                if release.artifacts.is_empty() {
                    bail!(
                        "stratum '{}': plugin '{}' release '{}' has no artifacts",
                        stratum.id,
                        plugin.id,
                        release.version
                    );
                }
                for artifact in &release.artifacts {
                    if !stratum.targets.contains(&artifact.runtime.trim()) {
                        bail!(
                            "stratum '{}' ({}) rejects the whole catalog on artifact runtime \
                             '{}' (plugin '{}' release '{}'); it parses only {}",
                            stratum.id,
                            stratum.label,
                            artifact.runtime,
                            plugin.id,
                            release.version,
                            stratum.targets.join(", ")
                        );
                    }
                    for feature in &artifact.required_features {
                        if !stratum.features.contains(feature) {
                            bail!(
                                "stratum '{}' ({}) rejects the whole catalog on required feature \
                                 '{}' (plugin '{}' release '{}')",
                                stratum.id,
                                stratum.label,
                                feature.as_str(),
                                plugin.id,
                                release.version
                            );
                        }
                    }
                    if artifact
                        .required_features
                        .contains(&WasmRequiredFeature::RelaxedSimd)
                        && !artifact
                            .required_features
                            .contains(&WasmRequiredFeature::Simd128)
                    {
                        bail!(
                            "stratum '{}': plugin '{}' release '{}' requires relaxed-simd without \
                             simd128, which every shipped client rejects",
                            stratum.id,
                            plugin.id,
                            release.version
                        );
                    }
                    let url = artifact.url.trim().to_ascii_lowercase();
                    if !(url.ends_with(".zst") || url.ends_with(".br")) {
                        bail!(
                            "stratum '{}': plugin '{}' release '{}' artifact '{}' has an encoding \
                             shipped clients reject",
                            stratum.id,
                            plugin.id,
                            release.version,
                            artifact.url
                        );
                    }
                    if artifact.digests.is_empty() || artifact.wasm_digests.is_empty() {
                        bail!(
                            "stratum '{}': plugin '{}' release '{}' artifact '{}' is missing \
                             digests",
                            stratum.id,
                            plugin.id,
                            release.version,
                            artifact.url
                        );
                    }
                    if artifact.bytes == 0 {
                        bail!(
                            "stratum '{}': plugin '{}' release '{}' artifact '{}' declares zero \
                             bytes",
                            stratum.id,
                            plugin.id,
                            release.version,
                            artifact.url
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CatalogV3RedirectArtifact, PluginCatalogStatus, RequiredSignerV2};

    fn artifact(
        runtime: &str,
        required_features: &[WasmRequiredFeature],
    ) -> CatalogV3PluginArtifact {
        let stem = if required_features.is_empty() {
            "plugin".to_string()
        } else {
            format!(
                "plugin-{}",
                required_features
                    .iter()
                    .map(|feature| feature.as_str())
                    .collect::<Vec<_>>()
                    .join("-")
            )
        };
        CatalogV3PluginArtifact {
            runtime: runtime.to_string(),
            required_features: required_features.to_vec(),
            wasm_digests: vec!["blake3:22".to_string()],
            bytes: 1234,
            url: format!("https://cdn.scryer.media/plugins-v3/alpha/{runtime}/{stem}.wasm.zst"),
            mirror_urls: Vec::new(),
            signature_url: format!(
                "https://cdn.scryer.media/plugins-v3/alpha/{runtime}/{stem}.wasm.zst.bundle.zst"
            ),
            signature_mirror_urls: Vec::new(),
            digests: vec!["blake3:11".to_string()],
        }
    }

    fn release(
        version: &str,
        max_scryer_version: Option<&str>,
        artifacts: Vec<CatalogV3PluginArtifact>,
    ) -> CatalogV3Release {
        CatalogV3Release {
            version: version.to_string(),
            sdk_constraint: ">=3.10.0, <4.0.0".to_string(),
            min_scryer_version: None,
            max_scryer_version: max_scryer_version.map(str::to_string),
            artifacts,
        }
    }

    fn plugin(id: &str, releases: Vec<CatalogV3Release>) -> CatalogV3PluginEntry {
        plugin_typed(id, "indexer", releases)
    }

    fn plugin_typed(
        id: &str,
        plugin_type: &str,
        releases: Vec<CatalogV3Release>,
    ) -> CatalogV3PluginEntry {
        CatalogV3PluginEntry {
            id: id.to_string(),
            name: id.to_string(),
            description: format!("{id} plugin"),
            plugin_type: plugin_type.to_string(),
            provider_type: id.to_string(),
            publisher: "scryer".to_string(),
            support_tier: "official".to_string(),
            status: PluginCatalogStatus::Active,
            docs_url: "https://github.com/scryer-media/scryer-plugins".to_string(),
            source_repo: "https://github.com/scryer-media/scryer-plugins".to_string(),
            required_signer: RequiredSignerV2 {
                github_repository: "scryer-media/scryer-plugins".to_string(),
                github_workflow: None,
            },
            releases,
        }
    }

    fn catalog(plugins: Vec<CatalogV3PluginEntry>) -> CatalogV3 {
        CatalogV3 {
            schema_version: crate::CATALOG_V3_SCHEMA.to_string(),
            catalog_version: 7,
            plugins,
            community_sources: Vec::new(),
            rule_packs: Vec::new(),
        }
    }

    /// The catalog the imminent component re-cut would produce: the last
    /// Preview 1 release retained, a new component release on top.
    fn component_recut_catalog() -> CatalogV3 {
        catalog(vec![plugin(
            "newznab",
            vec![
                release("2.0.3", Some("0.19.5"), vec![artifact(TARGET_WASIP1, &[])]),
                release("2.1.0", None, vec![artifact(TARGET_WASIP2, &[])]),
            ],
        )])
    }

    #[test]
    fn the_first_rung_drops_component_artifacts_and_keeps_the_preview1_release() {
        let master = component_recut_catalog();
        let stratum = stratum_by_id("legacy-no-release-ceiling").expect("stratum");

        let projection = project_catalog_for_stratum(&master, stratum);
        let releases = &projection.plugins[0].releases;
        assert_eq!(releases.len(), 1, "0.18.11 sees only the Preview 1 release");
        assert_eq!(releases[0].version, "2.0.3");
        assert!(
            releases[0]
                .artifacts
                .iter()
                .all(|artifact| artifact.runtime == TARGET_WASIP1)
        );
    }

    /// Regression pin against the live catalog: `catalog_version` 48 serves
    /// wasip2 indexer rows (gated `min_scryer_version: "0.18.22"`) on the
    /// `.last()` rung and 0.19.x installs run them. The shipped projection
    /// must not take that away.
    #[test]
    fn the_shipped_rung_keeps_the_wasip2_indexer_rows_it_serves_today() {
        let master = component_recut_catalog();
        let stratum = stratum_by_id("shipped-preview2").expect("stratum");

        let projection = project_catalog_for_stratum(&master, stratum);
        let releases = &projection.plugins[0].releases;
        assert_eq!(
            releases.len(),
            2,
            "0.19.x keeps both the Preview 1 release and the wasip2 indexer release"
        );
        assert_eq!(releases[1].artifacts[0].runtime, TARGET_WASIP2);
    }

    /// 0.19.x parses a wasip2 row for any family but can only *execute*
    /// indexer components — its component host speaks the `scryer:indexer`
    /// worlds and nothing else. A wasip2 row for another family would be
    /// selected, downloaded, and then fail to load, so the projection keeps it
    /// off the shipped rung entirely.
    #[test]
    fn a_non_indexer_component_never_reaches_the_shipped_rung() {
        let master = catalog(vec![
            plugin_typed(
                "enhanced-sync",
                "subtitle_provider",
                vec![
                    release("0.2.11", None, vec![artifact(TARGET_WASIP1, &[])]),
                    release("0.3.0", None, vec![artifact(TARGET_WASIP2, &[])]),
                ],
            ),
            plugin_typed(
                "archive-extraction",
                "archive_extractor",
                vec![release("1.0.0", None, vec![artifact(TARGET_WASIP2, &[])])],
            ),
        ]);
        let stratum = stratum_by_id("shipped-preview2").expect("stratum");

        let projection = project_catalog_for_stratum(&master, stratum);

        assert_eq!(
            projection.plugins.len(),
            1,
            "the component-only archive extractor has nothing 0.19.x can run"
        );
        let releases = &projection.plugins[0].releases;
        assert_eq!(
            releases.len(),
            1,
            "only the Preview 1 subtitle release survives"
        );
        assert_eq!(releases[0].version, "0.2.11");
    }

    #[test]
    fn the_pre_ceiling_stratum_also_loses_max_scryer_version() {
        let master = component_recut_catalog();
        let stratum = stratum_by_id("legacy-no-release-ceiling").expect("stratum");

        let projection = project_catalog_for_stratum(&master, stratum);

        assert!(
            projection.plugins[0].releases[0]
                .max_scryer_version
                .is_none(),
            "0.18.11 and older deny unknown fields; the field must be stripped, not left"
        );
    }

    #[test]
    fn the_modern_stratum_takes_the_master_catalog_unfiltered() {
        let master = component_recut_catalog();
        let stratum = stratum_by_id("modern").expect("stratum");

        let projection = project_catalog_for_stratum(&master, stratum);

        assert_eq!(projection.plugins[0].releases.len(), 2);
        assert_eq!(
            projection.plugins[0].releases[1].artifacts[0].runtime,
            TARGET_WASIP2
        );
    }

    #[test]
    fn the_component_recut_publishes_cleanly_against_the_published_catalog() {
        let existing = catalog(vec![plugin(
            "newznab",
            vec![release("2.0.3", None, vec![artifact(TARGET_WASIP1, &[])])],
        )]);

        let projections = build_publication_strata(
            &component_recut_catalog(),
            Some(&existing),
            &BTreeSet::new(),
        )
        .expect("retaining the Preview 1 release must publish cleanly");

        assert_eq!(projections.len(), CLIENT_STRATA.len());
    }

    /// The August 28 cut, replayed: drop the Preview 1 release and publish only
    /// components. This is the publish that took the catalog offline. The
    /// shipped rung tolerates it now (0.19.x runs wasip2 indexers), so the
    /// objection comes from the first rung, whose band cannot parse it.
    #[test]
    fn dropping_the_last_preview1_release_fails_the_render() {
        let existing = catalog(vec![plugin(
            "newznab",
            vec![release("2.0.3", None, vec![artifact(TARGET_WASIP1, &[])])],
        )]);
        let components_only = catalog(vec![plugin(
            "newznab",
            vec![release("2.1.0", None, vec![artifact(TARGET_WASIP2, &[])])],
        )]);

        let error = build_publication_strata(&components_only, Some(&existing), &BTreeSet::new())
            .expect_err("a components-only catalog must not be publishable silently");
        let message = error.to_string();

        assert!(
            message.contains("empty") || message.contains("strand"),
            "unexpected guard message: {message}"
        );
        assert!(message.contains("newznab") || message.contains("legacy-no-release-ceiling"));
    }

    #[test]
    fn a_stranding_is_publishable_only_with_an_explicit_acknowledgement() {
        let existing = catalog(vec![
            plugin(
                "newznab",
                vec![release("2.0.3", None, vec![artifact(TARGET_WASIP1, &[])])],
            ),
            plugin(
                "retired",
                vec![release("1.0.0", None, vec![artifact(TARGET_WASIP1, &[])])],
            ),
        ]);
        let candidate = catalog(vec![plugin(
            "newznab",
            vec![
                release("2.0.3", None, vec![artifact(TARGET_WASIP1, &[])]),
                release("2.1.0", None, vec![artifact(TARGET_WASIP2, &[])]),
            ],
        )]);

        let error = build_publication_strata(&candidate, Some(&existing), &BTreeSet::new())
            .expect_err("dropping 'retired' strands both legacy strata");
        let message = error.to_string();
        assert!(message.contains("retired@legacy-no-release-ceiling"));
        assert!(message.contains("retired@shipped-preview2"));
        assert!(
            !message.contains("retired@modern"),
            "the modern stratum takes the master catalog, so it is never stranded — an outright \
             removal is --allow-release-removal's business"
        );

        let allowed = parse_stratum_drops(&[
            "retired@legacy-no-release-ceiling".to_string(),
            "retired@shipped-preview2".to_string(),
        ])
        .expect("drop keys parse");
        build_publication_strata(&candidate, Some(&existing), &allowed)
            .expect("an acknowledged removal publishes");
    }

    #[test]
    fn the_wasip3_cut_keeps_a_wasip2_artifact_or_it_does_not_publish() {
        // Modern clients are tolerant, so the wasip3 cut is data only — as long
        // as each release still carries an artifact a wasip2 host can run.
        let master = catalog(vec![plugin(
            "newznab",
            vec![
                release("2.0.3", Some("0.19.5"), vec![artifact(TARGET_WASIP1, &[])]),
                release(
                    "3.0.0",
                    None,
                    vec![artifact(TARGET_WASIP2, &[]), artifact("wasm32-wasip3", &[])],
                ),
            ],
        )]);

        let projections = build_publication_strata(&master, None, &BTreeSet::new())
            .expect("a p2+p3 release publishes");
        let modern = projections
            .iter()
            .find(|projection| projection.stratum.slot == RedirectSlot::Modern)
            .expect("modern projection");
        assert_eq!(modern.catalog.plugins[0].releases[1].artifacts.len(), 2);

        let first_rung = projections
            .iter()
            .find(|projection| projection.stratum.slot == RedirectSlot::LegacyFirst)
            .expect("first rung");
        assert!(
            first_rung
                .catalog
                .plugins
                .iter()
                .flat_map(|plugin| &plugin.releases)
                .flat_map(|release| &release.artifacts)
                .all(|artifact| artifact.runtime == TARGET_WASIP1),
            "the first rung must never see a p2 or p3 row"
        );

        let shipped = projections
            .iter()
            .find(|projection| projection.stratum.slot == RedirectSlot::LegacyLast)
            .expect("shipped rung");
        let shipped_runtimes = shipped
            .catalog
            .plugins
            .iter()
            .flat_map(|plugin| &plugin.releases)
            .flat_map(|release| &release.artifacts)
            .map(|artifact| artifact.runtime.as_str())
            .collect::<BTreeSet<_>>();
        assert!(
            shipped_runtimes.contains(TARGET_WASIP2),
            "the shipped rung keeps the wasip2 indexer row"
        );
        assert!(
            !shipped_runtimes.contains("wasm32-wasip3"),
            "a wasip3 row on the shipped rung would freeze every 0.19.x install \
             exactly the way wasip2 froze 0.18.x"
        );
    }

    #[test]
    fn a_shipped_parser_rejects_a_projection_that_still_carries_a_future_runtime() {
        // The 0.18.x pinned validator rejects wasip2 — this is the August 28
        // failure, replayed as a render-time error.
        let first_rung = stratum_by_id("legacy-no-release-ceiling").expect("stratum");
        let wasip2_leak = catalog(vec![plugin(
            "newznab",
            vec![release("2.1.0", None, vec![artifact(TARGET_WASIP2, &[])])],
        )]);
        let error = shipped_parsers::validate_as_stratum(first_rung, &wasip2_leak)
            .expect_err("the pinned 0.18.x validator must reject a wasip2 row");
        assert!(error.to_string().contains("rejects the whole catalog"));

        // The 0.19.x pinned validator accepts wasip2 but rejects anything
        // newer — the same failure shape, one runtime later.
        let shipped = stratum_by_id("shipped-preview2").expect("stratum");
        let wasip2_fine = catalog(vec![plugin(
            "newznab",
            vec![release("2.1.0", None, vec![artifact(TARGET_WASIP2, &[])])],
        )]);
        shipped_parsers::validate_as_stratum(shipped, &wasip2_fine)
            .expect("the pinned 0.19.x validator accepts a wasip2 row");

        let wasip3_leak = catalog(vec![plugin(
            "newznab",
            vec![release("3.0.0", None, vec![artifact("wasm32-wasip3", &[])])],
        )]);
        let error = shipped_parsers::validate_as_stratum(shipped, &wasip3_leak)
            .expect_err("the pinned 0.19.x validator must reject a wasip3 row");
        assert!(error.to_string().contains("rejects the whole catalog"));
    }

    #[test]
    fn stratum_drop_keys_must_name_a_known_stratum() {
        assert!(parse_stratum_drop("newznab@shipped-preview2").is_ok());
        assert!(parse_stratum_drop("newznab@does-not-exist").is_err());
        assert!(parse_stratum_drop("no-stratum").is_err());
        assert!(parse_stratum_drop("@shipped-preview2").is_err());
    }

    #[test]
    fn redirect_ordering_puts_each_stratum_on_the_rung_its_client_reads() {
        let projections =
            build_publication_strata(&component_recut_catalog(), None, &BTreeSet::new())
                .expect("projections");
        let legacy = projections
            .iter()
            .filter(|projection| projection.stratum.slot.is_legacy())
            .collect::<Vec<_>>();

        assert_eq!(
            legacy.first().expect("first rung").stratum.slot,
            RedirectSlot::LegacyFirst
        );
        assert_eq!(
            legacy.last().expect("last rung").stratum.slot,
            RedirectSlot::LegacyLast
        );

        // Sanity: a redirect built from these rungs keeps that order.
        let ladder = legacy
            .iter()
            .map(|projection| CatalogV3RedirectArtifact {
                url: format!(
                    "https://cdn.scryer.media/{}.json.zst",
                    projection.stratum.id
                ),
                mirror_urls: Vec::new(),
                signature_url: format!(
                    "https://cdn.scryer.media/{}.json.zst.bundle",
                    projection.stratum.id
                ),
                signature_mirror_urls: Vec::new(),
            })
            .collect::<Vec<_>>();
        assert!(ladder[0].url.contains("legacy-no-release-ceiling"));
        assert!(ladder[ladder.len() - 1].url.contains("shipped-preview2"));
    }
}
