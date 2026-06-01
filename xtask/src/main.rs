use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::{Args, Parser, Subcommand, ValueEnum};
use extism::{Manifest, UserData, ValType, host_fn};
mod plugin_new;
use scryer_plugin_sdk::{
    EXPORT_DESCRIBE, EXPORT_DOWNLOAD_ADD, EXPORT_DOWNLOAD_CONTROL, EXPORT_DOWNLOAD_LIST_COMPLETED,
    EXPORT_DOWNLOAD_LIST_HISTORY, EXPORT_DOWNLOAD_LIST_QUEUE, EXPORT_DOWNLOAD_MARK_IMPORTED,
    EXPORT_DOWNLOAD_STATUS, EXPORT_DOWNLOAD_TEST_CONNECTION, EXPORT_INDEXER_SEARCH,
    EXPORT_NOTIFICATION_SEND, EXPORT_SUBSYNC_ALIGN, EXPORT_SUBTITLE_DOWNLOAD,
    EXPORT_SUBTITLE_GENERATE, EXPORT_SUBTITLE_SEARCH, EXPORT_VALIDATE_CONFIG, PluginDescriptor,
    PluginResult, ProviderDescriptor, SDK_VERSION, SubtitleProviderMode, SubtitleSyncAlignRequest,
    SubtitleSyncAlignResponse, SubtitleSyncInputSubtitle, SubtitleSyncReferenceSubtitle,
    host_version_matches_constraint, plugin_descriptor_sdk_constraint,
    validate_plugin_descriptor_host_permissions, validate_sdk_contract,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::BufWriter;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use toml_edit::{DocumentMut, value};

const BLUE: &str = "\x1b[0;34m";
const GREEN: &str = "\x1b[0;32m";
const YELLOW: &str = "\x1b[1;33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const WASM_TARGET: &str = "wasm32-wasip1";
const CATALOG_V2_SCHEMA: &str = "scryer.plugin.catalog.v2";
const CHILD_CATALOG_V2_SCHEMA: &str = "scryer.plugin.child_catalog.v2";
const CATALOG_V3_SCHEMA: &str = "scryer.plugin.catalog.v3";
const CATALOG_V3_REDIRECT_SCHEMA: &str = "scryer.plugin.catalog.v3.redirect";
const PLUGIN_MANIFEST_SCHEMA: &str = "scryer.plugin.v1";
const WASM_OPT_LEVEL_SIZE: &str = "-Oz";
const WASM_OPT_LEVEL_SPEED: &str = "-O3";
const ZSTD_LEVEL: &str = "-19";
const SHORT_CATALOG_HASH_LEN: usize = 12;
const OFFICIAL_GITHUB_REPO: &str = "scryer-media/scryer-plugins";
const DEFAULT_OFFICIAL_RELEASE_WORKFLOW: &str = ".github/workflows/release-plugin.yml";
const OFFICIAL_RELEASE_WORKFLOW_ENV: &str = "SCRYER_OFFICIAL_RELEASE_WORKFLOW_PATH";
const OFFICIAL_PLUGIN_RELEASE_TAG_PREFIX_ENV: &str = "SCRYER_OFFICIAL_PLUGIN_RELEASE_TAG_PREFIX";
const DEFAULT_OFFICIAL_PLUGIN_RELEASE_TAG_PREFIX: &str = "plugins";
const RELEASE_SOURCE_ROOT_ENV: &str = "SCRYER_PLUGIN_RELEASE_SOURCE_ROOT";
const SDK_LOCAL_OVERRIDE_ENV: &str = "SCRYER_PLUGIN_SDK_LOCAL_PATH";
const CENTRAL_CATALOG_RELEASE_TAG: &str = "catalog/v2";
const DEFAULT_CENTRAL_CATALOG_V3_RELEASE_TAG: &str = "catalog/v3";
const CENTRAL_CATALOG_V3_RELEASE_TAG_ENV: &str = "SCRYER_CATALOG_V3_RELEASE_TAG";
const DEFAULT_CENTRAL_CATALOG_V3_PATH_PREFIX: &str = "catalog/v3";
const CENTRAL_CATALOG_V3_PATH_PREFIX_ENV: &str = "SCRYER_CATALOG_V3_PATH_PREFIX";
const CATALOG_V2_BASE_SDK_VERSION: &str = "1.5.0";
const RULE_PACK_SOURCE_MANIFEST: &str = "rule_packs/manifest.json";
const REPO_RELEASE_TAG_PREFIX: &str = "plugins/release/";
const CATALOG_PRETTY_JSON: &str = "catalog-v2.json";
const CATALOG_MINIFIED_JSON: &str = "catalog-v2.min.json";
const CATALOG_MINIFIED_ZST: &str = "catalog-v2.min.json.zst";
const CATALOG_V3_SNIPPET_JSON: &str = "catalog-v3.json";
const CATALOG_V3_MINIFIED_JSON: &str = "catalog-v3.min.json";
const CATALOG_V3_MINIFIED_ZST: &str = "catalog-v3.min.json.zst";
const CATALOG_V3_REDIRECT_JSON: &str = "catalog-v3.redirect.json";
const R2_ACCOUNT_ID_ENV: &str = "CF_ACCOUNT_ID";
const R2_ACCOUNT_ID_ENV_LEGACY: &str = "CF_R2_ACCOUNT_ID";
const R2_BUCKET_ENV: &str = "CF_R2_BUCKET_ID";
const R2_BUCKET_ENV_LEGACY: &str = "CF_R2_BUCKET";
const R2_ACCESS_KEY_ID_ENV: &str = "CF_R2_ACCESS_KEY_ID";
const R2_SECRET_ACCESS_KEY_ENV: &str = "CF_R2_SECRET_ACCESS_KEY";
const R2_UPLOAD_ENDPOINT_ENV: &str = "CF_JURISDICTION_URL";
const R2_PUBLIC_BASE_URL_ENV: &str = "CF_R2_PUBLIC_BASE_URL";
const DEFAULT_R2_PUBLIC_BASE_URL: &str = "https://cdn.scryer.media/scryer";
const BROTLI_QUALITY: u32 = 11;
const BROTLI_LGWIN: u32 = 24;
const ENHANCED_SYNC_FFMPEG_VENDOR_DIR: &str = "subtitles/enhanced-sync/vendor/ffmpeg";
const ENHANCED_SYNC_FFMPEG_VENDOR_ARCHIVE: &str = "source.tar.zst";
const ENHANCED_SYNC_FFMPEG_VENDOR_METADATA: &str = "SCRYER_VENDOR_METADATA";
const ENHANCED_SYNC_LIBFVAD_VENDOR_DIR: &str = "subtitles/enhanced-sync/vendor/libfvad";
const ENHANCED_SYNC_LIBFVAD_VENDOR_ARCHIVE: &str = "source.tar.zst";
const ENHANCED_SYNC_LIBFVAD_VENDOR_METADATA: &str = "SCRYER_VENDOR_METADATA";
const ENHANCED_SUBTITLE_SYNC_PLUGIN_ID: &str = "enhanced-subtitle-sync";
const SUBTITLE_SYNC_PARITY_FORMATS: &[&str] = &["srt", "vtt", "ass", "ssa"];
const SUBTITLE_SYNC_FLOAT_TOLERANCE: f64 = 1.0e-9;
const FFMPEG_VENDOR_PATHS: &[&str] = &[
    "COPYING.LGPLv2.1",
    "LICENSE.md",
    "Makefile",
    "RELEASE",
    "compat",
    "configure",
    "doc",
    "ffbuild",
    "fftools",
    "libavcodec",
    "libavdevice",
    "libavfilter",
    "libavformat",
    "libavutil",
    "libswresample",
    "tests",
    "tools",
];
const LIBFVAD_VENDOR_PATHS: &[&str] = &[
    "AUTHORS",
    "LICENSE",
    "PATENTS",
    "README.md",
    "include",
    "src",
];
const AUDIT_IGNORE_ADVISORIES: &[&str] = &[
    // Extism currently pins wasmtime 41.x upstream, so these remain blocked on
    // the runtime stack moving onto a patched line.
    "RUSTSEC-2026-0085",
    "RUSTSEC-2026-0086",
    "RUSTSEC-2026-0087",
    "RUSTSEC-2026-0088",
    "RUSTSEC-2026-0089",
    "RUSTSEC-2026-0091",
    "RUSTSEC-2026-0092",
    "RUSTSEC-2026-0093",
    "RUSTSEC-2026-0094",
    "RUSTSEC-2026-0095",
    "RUSTSEC-2026-0096",
    "RUSTSEC-2026-0114",
];

host_fn!(socket_unsupported(_state: (); _input: String) -> String {
    Ok(
        r#"{"ok":false,"error":{"code":"unsupported","message":"socket host calls are unavailable during descriptor validation"}}"#
            .to_string(),
    )
});

#[derive(Clone)]
struct RustupToolchain {
    rustup: PathBuf,
    toolchain: String,
}

fn repo_root() -> PathBuf {
    if let Ok(path) = env::var(RELEASE_SOURCE_ROOT_ENV) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a repo root parent")
        .to_path_buf()
}

#[derive(Parser)]
#[command(name = "cargo xtask")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Doctor,
    Ci(CiArgs),
    Release(ReleaseArgs),
    ReleaseMany(ReleaseManyArgs),
    ReleaseChanged(ReleaseChangedArgs),
    Plugin(PluginArgs),
    Ffmpeg(FfmpegArgs),
    Vad(VadArgs),
    Sdk(SdkArgs),
    Official(OfficialArgs),
    Catalog(CatalogArgs),
    Community(CommunityArgs),
}

#[derive(Args, Clone, Default)]
struct ReleaseOptions {
    #[arg(long, conflicts_with_all = ["minor", "patch", "version"])]
    major: bool,
    #[arg(long, conflicts_with_all = ["major", "patch", "version"])]
    minor: bool,
    #[arg(long, conflicts_with_all = ["major", "minor", "version"])]
    patch: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    version: Option<String>,
}

#[derive(Args)]
struct ReleaseChangedArgs {
    #[command(flatten)]
    options: ReleaseOptions,
}

#[derive(Args)]
struct ReleaseArgs {
    plugin_name: String,
    #[command(flatten)]
    options: ReleaseOptions,
}

#[derive(Args)]
struct ReleaseManyArgs {
    plugin_names: Vec<String>,
    #[command(flatten)]
    options: ReleaseOptions,
}

#[derive(Args)]
struct CiArgs {
    #[command(subcommand)]
    command: CiCommand,
}

#[derive(Args, Clone, Default)]
struct CiScopeArgs {
    #[arg(long = "plugin-id")]
    plugin_ids: Vec<String>,
}

#[derive(Subcommand)]
enum CiCommand {
    Fmt(CiScopeArgs),
    Clippy(CiScopeArgs),
    Audit(CiScopeArgs),
    Strict(CiScopeArgs),
}

#[derive(Args)]
struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Subcommand)]
enum PluginCommand {
    New(PluginNewArgs),
    Validate(PluginValidateArgs),
    BuildAll,
    ValidateAll,
}

#[derive(Args)]
struct PluginNewArgs {
    kind: PluginKindArg,
    name: String,
}

#[derive(Args)]
struct PluginValidateArgs {
    path: PathBuf,
}

#[derive(Args)]
struct FfmpegArgs {
    #[command(subcommand)]
    command: FfmpegCommand,
}

#[derive(Subcommand)]
enum FfmpegCommand {
    Revendor(FfmpegRevendorArgs),
}

#[derive(Args)]
struct FfmpegRevendorArgs {
    #[arg(long, default_value = "https://github.com/FFmpeg/FFmpeg.git")]
    source: String,
    #[arg(long)]
    commit: String,
}

#[derive(Args)]
struct VadArgs {
    #[command(subcommand)]
    command: VadCommand,
}

#[derive(Subcommand)]
enum VadCommand {
    Revendor(VadRevendorArgs),
}

#[derive(Args)]
struct VadRevendorArgs {
    #[arg(long, default_value = "https://github.com/dpirch/libfvad.git")]
    source: String,
    #[arg(long)]
    commit: String,
}

#[derive(Args)]
struct SdkArgs {
    #[command(subcommand)]
    command: SdkCommand,
}

#[derive(Subcommand)]
enum SdkCommand {
    Bump { version: String },
}

#[derive(Args)]
struct OfficialArgs {
    #[command(subcommand)]
    command: OfficialCommand,
}

#[derive(Subcommand)]
enum OfficialCommand {
    Release(OfficialReleaseArgs),
    Prepare(OfficialPrepareArgs),
    Prefetch(OfficialPrefetchArgs),
    PlanChanged(OfficialPlanChangedArgs),
    PlanCurrent(OfficialPlanCurrentArgs),
    VerifyPrepared(OfficialVerifyPreparedArgs),
    UploadR2(OfficialUploadR2Args),
}

#[derive(Args)]
struct OfficialReleaseArgs {
    plugin_id: String,
    #[arg(long)]
    version: Option<String>,
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    existing_child_catalog: Option<PathBuf>,
}

#[derive(Args)]
struct OfficialPrepareArgs {
    plugin_id: String,
    #[arg(long)]
    version: Option<String>,
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    existing_child_catalog: Option<PathBuf>,
    #[arg(long, value_enum)]
    catalog_version: Option<CatalogVersion>,
}

#[derive(Args)]
struct OfficialPrefetchArgs {
    plugin_ids: Vec<String>,
}

#[derive(Args)]
struct OfficialPlanCurrentArgs {
    plugin_ids: Vec<String>,
}

#[derive(Args, Clone, Default)]
struct OfficialPlanChangedArgs {
    #[arg(long, conflicts_with_all = ["minor", "patch", "version"])]
    major: bool,
    #[arg(long, conflicts_with_all = ["major", "patch", "version"])]
    minor: bool,
    #[arg(long, conflicts_with_all = ["major", "minor", "version"])]
    patch: bool,
    #[arg(long)]
    version: Option<String>,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args)]
struct OfficialVerifyPreparedArgs {
    dir: PathBuf,
}

#[derive(Args)]
struct OfficialUploadR2Args {
    dir: PathBuf,
}

#[derive(Args)]
struct CatalogArgs {
    #[command(subcommand)]
    command: CatalogCommand,
}

#[derive(Subcommand)]
enum CatalogCommand {
    RenderV2,
    RenderV3,
    PrepareV2(CatalogPrepareV2Args),
    PrepareV3(CatalogPrepareV3Args),
    PublishV2,
    PublishV3,
    UploadV3R2(CatalogUploadV3R2Args),
    ValidateV2,
}

#[derive(Args)]
struct CatalogPrepareV2Args {
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long = "plugin-id")]
    plugin_ids: Vec<String>,
    #[arg(long)]
    existing_catalog: Option<PathBuf>,
    #[arg(long)]
    prepared_child_catalog_root: Option<PathBuf>,
}

#[derive(Args)]
struct CatalogPrepareV3Args {
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long = "plugin-id")]
    plugin_ids: Vec<String>,
    #[arg(long)]
    existing_catalog: Option<PathBuf>,
    #[arg(long)]
    prepared_plugin_root: Option<PathBuf>,
    #[arg(long, hide = true)]
    allow_selected_rebuild: bool,
}

#[derive(Args)]
struct CatalogUploadV3R2Args {
    dir: PathBuf,
}

#[derive(Args)]
struct CommunityArgs {
    #[command(subcommand)]
    command: CommunityCommand,
}

#[derive(Subcommand)]
enum CommunityCommand {
    Scaffold {
        plugin_id: String,
        output_dir: PathBuf,
    },
    Approve {
        github_repo: String,
    },
    Verify {
        github_repo: String,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum PluginKindArg {
    Indexer,
    DownloadClient,
    Notification,
    Subtitle,
}

#[derive(Copy, Clone, Eq, PartialEq, ValueEnum)]
enum VersionBump {
    Patch,
    Minor,
    Major,
}

#[derive(Clone)]
struct TaskContext {
    repo_root: PathBuf,
}

#[derive(Clone)]
struct ReleaseTarget {
    plugin_id: String,
    plugin_dir: PathBuf,
    cargo_toml: PathBuf,
    crate_name: String,
    current_version: Version,
    next_version: Version,
    tag_name: String,
}

#[derive(Clone)]
struct PlannedReleaseTarget {
    target: ReleaseTarget,
    reason: String,
}

#[derive(Clone)]
struct LocalPluginInfo {
    plugin_id: String,
    name: String,
    description: String,
    plugin_type: String,
    provider_type: String,
    status: PluginCatalogStatus,
    catalog_versions: BTreeSet<CatalogVersion>,
    feature_sets: Vec<WasmFeatureSet>,
    min_scryer_version: Option<String>,
    docs_url: String,
    plugin_dir: PathBuf,
    cargo_toml: PathBuf,
    crate_name: String,
    current_version: Version,
    source_repo: String,
    distribution_base_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PluginManifestMetadata {
    description: String,
    official: bool,
    plugin_id: Option<String>,
    status: PluginCatalogStatus,
    catalog_versions: BTreeSet<CatalogVersion>,
    feature_sets: Vec<WasmFeatureSet>,
    min_scryer_version: Option<String>,
    docs_url: Option<String>,
    source_repo: Option<String>,
    distribution_base_url: Option<String>,
}

#[derive(Clone, Debug)]
struct CatalogAssetPaths {
    pretty_json: PathBuf,
    minified_json: PathBuf,
    minified_zst: PathBuf,
}

#[derive(Clone, Debug)]
struct CatalogV3AssetPaths {
    pretty_json: PathBuf,
    minified_json: PathBuf,
    minified_zst: PathBuf,
    redirect_json: PathBuf,
}

#[derive(Clone, Debug)]
struct OfficialPreparedRelease {
    dist: PathBuf,
    variants: Vec<PreparedPluginVariant>,
    manifest_json: Option<PathBuf>,
    child_catalog: Option<CatalogAssetPaths>,
    catalog_v3_snippet: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct PreparedCompressedArtifact {
    source_path: PathBuf,
    staged_path: Option<PathBuf>,
    digests: Vec<String>,
}

#[derive(Clone, Debug)]
struct PreparedPluginVariant {
    feature_set: WasmFeatureSet,
    optimized_wasm: PathBuf,
    bytes: u64,
    wasm_digests: Vec<String>,
    compressed_zst: PreparedCompressedArtifact,
    compressed_br: PreparedCompressedArtifact,
}

#[derive(Clone, Debug)]
struct BuiltPluginVariant {
    feature_set: WasmFeatureSet,
    wasm_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReleaseImpact {
    PluginChanged,
    ArtifactWide(String),
    Unchanged,
}

impl TaskContext {
    fn new() -> Self {
        Self {
            repo_root: repo_root(),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.repo_root.join(relative)
    }

    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        Command::new(program)
    }

    fn command_in(&self, program: impl AsRef<OsStr>, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command.current_dir(cwd);
        command
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV2 {
    schema_version: String,
    plugins: Vec<CatalogV2Entry>,
    #[serde(default)]
    rule_packs: Vec<RulePackCatalogEntryV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV2Entry {
    id: String,
    name: String,
    description: String,
    plugin_type: String,
    provider_type: String,
    publisher: String,
    support_tier: String,
    docs_url: String,
    source_repo: String,
    child_catalog_url: String,
    required_signer: RequiredSignerV2,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PluginCatalogStatus {
    Beta,
    Active,
    Deprecated,
}

impl PluginCatalogStatus {
    fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "beta" => Ok(Self::Beta),
            "active" => Ok(Self::Active),
            "deprecated" => Ok(Self::Deprecated),
            other => bail!("unsupported package.metadata.scryer.status value '{other}'"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ValueEnum, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
enum CatalogVersion {
    V2,
    V3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PluginArtifactLane {
    V2,
    V3,
}

impl CatalogVersion {
    fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "v2" => Ok(Self::V2),
            "v3" => Ok(Self::V3),
            other => bail!("unsupported package.metadata.scryer.catalog_versions value '{other}'"),
        }
    }
}

fn default_catalog_versions() -> BTreeSet<CatalogVersion> {
    BTreeSet::from([CatalogVersion::V2, CatalogVersion::V3])
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
enum WasmRequiredFeature {
    #[serde(rename = "simd128")]
    Simd128,
    #[serde(rename = "relaxed-simd")]
    RelaxedSimd,
}

impl WasmRequiredFeature {
    fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "simd128" => Ok(Self::Simd128),
            "relaxed-simd" => Ok(Self::RelaxedSimd),
            other => bail!(
                "unsupported package.metadata.scryer.feature_sets required_features value '{other}'"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Simd128 => "simd128",
            Self::RelaxedSimd => "relaxed-simd",
        }
    }

    fn rust_target_feature(self) -> &'static str {
        match self {
            Self::Simd128 => "simd128",
            Self::RelaxedSimd => "relaxed-simd",
        }
    }

    fn wasm_opt_flag(self) -> &'static str {
        match self {
            Self::Simd128 => "--enable-simd",
            Self::RelaxedSimd => "--enable-relaxed-simd",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct WasmFeatureSet {
    required_features: Vec<WasmRequiredFeature>,
}

impl WasmFeatureSet {
    fn new(mut required_features: Vec<WasmRequiredFeature>) -> Self {
        required_features.sort();
        required_features.dedup();
        Self { required_features }
    }

    fn baseline() -> Self {
        Self {
            required_features: Vec::new(),
        }
    }

    fn is_baseline(&self) -> bool {
        self.required_features.is_empty()
    }

    fn slug(&self) -> String {
        if self.is_baseline() {
            "baseline".to_string()
        } else {
            self.required_features
                .iter()
                .map(|feature| feature.as_str())
                .collect::<Vec<_>>()
                .join("-")
        }
    }

    fn artifact_stem(&self) -> String {
        if self.is_baseline() {
            "plugin".to_string()
        } else {
            format!("plugin-{}", self.slug())
        }
    }

    fn target_dir_component(&self) -> String {
        self.slug()
    }

    fn required_features_env_value(&self) -> String {
        self.required_features
            .iter()
            .map(|feature| feature.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    fn validate(&self) -> Result<()> {
        let has_relaxed_simd = self
            .required_features
            .contains(&WasmRequiredFeature::RelaxedSimd);
        let has_simd128 = self
            .required_features
            .contains(&WasmRequiredFeature::Simd128);
        if has_relaxed_simd && !has_simd128 {
            bail!("relaxed-simd requires simd128 in package.metadata.scryer.feature_sets");
        }
        Ok(())
    }

    fn rust_target_feature_flag(&self) -> Option<String> {
        if self.required_features.is_empty() {
            return None;
        }

        Some(format!(
            "-C target-feature=+{}",
            self.required_features
                .iter()
                .map(|feature| feature.rust_target_feature())
                .collect::<Vec<_>>()
                .join(",+")
        ))
    }

    fn wasm_opt_level(&self) -> &'static str {
        if self.is_baseline() {
            WASM_OPT_LEVEL_SIZE
        } else {
            WASM_OPT_LEVEL_SPEED
        }
    }
}

fn default_feature_sets() -> Vec<WasmFeatureSet> {
    vec![WasmFeatureSet::baseline()]
}

fn parse_feature_sets(
    manifest_path: &Path,
    scryer_metadata: Option<&toml_edit::Item>,
) -> Result<Vec<WasmFeatureSet>> {
    let Some(values) = scryer_metadata
        .and_then(|scryer| scryer.get("feature_sets"))
        .and_then(|value| value.as_array())
    else {
        return Ok(default_feature_sets());
    };

    let mut parsed = Vec::with_capacity(values.len());
    for value in values.iter() {
        let required_features = value
            .as_inline_table()
            .and_then(|table| table.get("required_features"))
            .and_then(|required_features| required_features.as_array())
            .ok_or_else(|| {
                anyhow!(
                    "{} package.metadata.scryer.feature_sets entries must be inline tables with required_features arrays",
                    manifest_path.display()
                )
            })?
            .iter()
            .map(|feature| {
                let feature = feature.as_str().ok_or_else(|| {
                    anyhow!(
                        "{} package.metadata.scryer.feature_sets required_features entries must be strings",
                        manifest_path.display()
                    )
                })?;
                WasmRequiredFeature::parse(feature)
            })
            .collect::<Result<Vec<_>>>()?;
        let feature_set = WasmFeatureSet::new(required_features);
        feature_set.validate()?;
        parsed.push(feature_set);
    }

    if parsed.is_empty() {
        bail!(
            "{} must define at least one package.metadata.scryer.feature_sets entry",
            manifest_path.display()
        );
    }

    parsed.sort();
    parsed.dedup();
    Ok(parsed)
}

fn feature_sets_include_baseline(feature_sets: &[WasmFeatureSet]) -> bool {
    feature_sets.iter().any(WasmFeatureSet::is_baseline)
}

fn primary_feature_set(feature_sets: &[WasmFeatureSet]) -> &WasmFeatureSet {
    feature_sets
        .iter()
        .find(|feature_set| feature_set.is_baseline())
        .unwrap_or_else(|| {
            feature_sets
                .first()
                .expect("feature_sets should never be empty")
        })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RequiredSignerV2 {
    github_repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    github_workflow: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RulePackCatalogEntryV2 {
    id: String,
    name: String,
    description: String,
    author: String,
    version: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_scryer_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct RulePackSourceManifest {
    rule_packs: Vec<RulePackSourceEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct RulePackSourceEntry {
    id: String,
    asset: String,
    distribution_base_url: String,
    #[serde(default)]
    min_scryer_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct RulePackManifestV1 {
    schema_version: u32,
    id: String,
    name: String,
    description: String,
    author: String,
    version: String,
    #[serde(default)]
    rules: Vec<serde_json::Value>,
}

#[derive(Clone, Debug)]
struct PreparedRulePack {
    entry: RulePackCatalogEntryV2,
    source_path: PathBuf,
    asset_name: String,
}

#[derive(Clone, Debug)]
struct PreparedRulePackV3 {
    entry: CatalogV3RulePackEntry,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChildCatalogV2 {
    schema_version: String,
    id: String,
    name: String,
    description: String,
    plugin_type: String,
    provider_type: String,
    publisher: String,
    support_tier: String,
    docs_url: String,
    source_repo: String,
    releases: Vec<ChildCatalogReleaseV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChildCatalogReleaseV2 {
    version: String,
    sdk_constraint: String,
    artifact_manifest_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3 {
    schema_version: String,
    #[serde(default)]
    catalog_version: u64,
    plugins: Vec<CatalogV3PluginEntry>,
    #[serde(default)]
    rule_packs: Vec<CatalogV3RulePackEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3PluginEntry {
    id: String,
    name: String,
    description: String,
    plugin_type: String,
    provider_type: String,
    publisher: String,
    support_tier: String,
    status: PluginCatalogStatus,
    docs_url: String,
    source_repo: String,
    required_signer: RequiredSignerV2,
    releases: Vec<CatalogV3Release>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3Release {
    version: String,
    sdk_constraint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_scryer_version: Option<String>,
    artifacts: Vec<CatalogV3PluginArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3Artifact {
    url: String,
    mirror_urls: Vec<String>,
    signature_url: String,
    signature_mirror_urls: Vec<String>,
    digests: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3PluginArtifact {
    runtime: String,
    #[serde(default)]
    required_features: Vec<WasmRequiredFeature>,
    wasm_digests: Vec<String>,
    bytes: u64,
    url: String,
    #[serde(default)]
    mirror_urls: Vec<String>,
    signature_url: String,
    #[serde(default)]
    signature_mirror_urls: Vec<String>,
    digests: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3RulePackEntry {
    id: String,
    name: String,
    description: String,
    author: String,
    releases: Vec<CatalogV3RulePackRelease>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3RulePackRelease {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_scryer_version: Option<String>,
    rule_pack_digests: Vec<String>,
    artifacts: Vec<CatalogV3Artifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3Redirect {
    schema_version: String,
    catalog_version: u64,
    artifacts: Vec<CatalogV3RedirectArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogV3RedirectArtifact {
    url: String,
    mirror_urls: Vec<String>,
    signature_url: String,
    signature_mirror_urls: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PluginManifestV2 {
    schema_version: String,
    id: String,
    plugin_type: String,
    provider_type: String,
    version: String,
    publisher: String,
    artifact: String,
    compression: String,
    wasm_digest: String,
    artifact_digest: String,
    signature: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let ctx = TaskContext::new();

    match cli.command {
        Commands::Doctor => run_doctor(&ctx),
        Commands::Ci(args) => match args.command {
            CiCommand::Fmt(args) => run_ci_fmt_check(&ctx, &args),
            CiCommand::Clippy(args) => run_ci_strict_clippy(&ctx, &args),
            CiCommand::Audit(args) => run_ci_audit(&ctx, &args),
            CiCommand::Strict(args) => run_ci_strict(&ctx, &args),
        },
        Commands::Release(args) => run_release(&ctx, args),
        Commands::ReleaseMany(args) => run_release_many(&ctx, args),
        Commands::ReleaseChanged(args) => run_release_changed(&ctx, args),
        Commands::Plugin(args) => match args.command {
            PluginCommand::New(args) => plugin_new::run_plugin_new(&ctx, args),
            PluginCommand::Validate(args) => run_plugin_validate(&ctx, args),
            PluginCommand::BuildAll => run_plugin_build_all(&ctx),
            PluginCommand::ValidateAll => run_plugin_validate_all(&ctx),
        },
        Commands::Ffmpeg(args) => match args.command {
            FfmpegCommand::Revendor(args) => run_ffmpeg_revendor(&ctx, args),
        },
        Commands::Vad(args) => match args.command {
            VadCommand::Revendor(args) => run_vad_revendor(&ctx, args),
        },
        Commands::Sdk(args) => match args.command {
            SdkCommand::Bump { version } => run_sdk_bump(&ctx, &version),
        },
        Commands::Official(args) => match args.command {
            OfficialCommand::Release(args) => run_official_release(&ctx, args),
            OfficialCommand::Prepare(args) => run_official_prepare(&ctx, args),
            OfficialCommand::Prefetch(args) => run_official_prefetch(&ctx, args),
            OfficialCommand::PlanChanged(args) => run_official_plan_changed(&ctx, args),
            OfficialCommand::PlanCurrent(args) => run_official_plan_current(&ctx, args),
            OfficialCommand::VerifyPrepared(args) => run_official_verify_prepared(&ctx, &args.dir),
            OfficialCommand::UploadR2(args) => run_official_upload_r2(&ctx, &args.dir),
        },
        Commands::Catalog(args) => match args.command {
            CatalogCommand::RenderV2 => run_catalog_render_v2(&ctx),
            CatalogCommand::RenderV3 => run_catalog_render_v3(&ctx),
            CatalogCommand::PrepareV2(args) => run_catalog_prepare_v2(&ctx, args),
            CatalogCommand::PrepareV3(args) => run_catalog_prepare_v3(&ctx, args),
            CatalogCommand::PublishV2 => run_catalog_publish_v2(&ctx),
            CatalogCommand::PublishV3 => run_catalog_publish_v3(&ctx),
            CatalogCommand::UploadV3R2(args) => run_catalog_upload_v3_r2(&ctx, &args.dir),
            CatalogCommand::ValidateV2 => run_catalog_validate_v2(&ctx),
        },
        Commands::Community(args) => match args.command {
            CommunityCommand::Scaffold {
                plugin_id,
                output_dir,
            } => run_community_scaffold(&ctx, &plugin_id, &output_dir),
            CommunityCommand::Approve { github_repo } => run_community_approve(&ctx, &github_repo),
            CommunityCommand::Verify { github_repo } => run_community_verify(&ctx, &github_repo),
        },
    }
}

fn step(message: impl AsRef<str>) {
    println!("\n{BLUE}{BOLD}▶  {}{RESET}", message.as_ref());
}

fn ok(message: impl AsRef<str>) {
    println!("   {GREEN}✓  {}{RESET}", message.as_ref());
}

fn warn(message: impl AsRef<str>) {
    eprintln!("   {YELLOW}⚠  {}{RESET}", message.as_ref());
}

fn run_status(command: &mut Command) -> Result<ExitStatus> {
    Ok(command.status()?)
}

fn run_checked(command: &mut Command) -> Result<()> {
    let debug = format!("{command:?}");
    let status = run_status(command)?;
    if !status.success() {
        bail!("command failed: {debug}");
    }
    Ok(())
}

fn run_capture(command: &mut Command) -> Result<String> {
    let debug = format!("{command:?}");
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command failed: {debug}\n{stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn rustup_toolchain_override() -> Option<String> {
    env::var("SCRYER_RUSTUP_TOOLCHAIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn rustup_binary_override() -> Option<PathBuf> {
    env::var_os("SCRYER_RUSTUP_BIN")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn rustup_binary() -> Option<PathBuf> {
    if let Some(path) = rustup_binary_override().filter(|path| path.is_file()) {
        return Some(path);
    }

    let exe = format!("rustup{}", env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|dir| dir.join(&exe)));
    }
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".cargo/bin").join(&exe));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(&exe));
    candidates.push(PathBuf::from("/usr/local/bin").join(&exe));
    candidates.into_iter().find(|path| path.is_file())
}

fn rustup_toolchain_from_file(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }

    let document = fs::read_to_string(path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(document["toolchain"]["channel"]
        .as_str()
        .map(ToOwned::to_owned))
}

fn validate_plugin_release_profile(cargo_toml: &Path) -> Result<()> {
    let document = read_manifest_document(cargo_toml)?;
    let profile = document
        .get("profile")
        .and_then(|value| value.get("plugin-release"))
        .ok_or_else(|| {
            anyhow!(
                "{} must define [profile.plugin-release]",
                cargo_toml.display()
            )
        })?;

    let inherits = profile
        .get("inherits")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            anyhow!(
                "{} must define profile.plugin-release.inherits",
                cargo_toml.display()
            )
        })?;
    if inherits != "release" {
        bail!(
            "{} must set profile.plugin-release.inherits = \"release\"",
            cargo_toml.display()
        );
    }

    let opt_level = profile
        .get("opt-level")
        .and_then(|value| value.as_integer())
        .ok_or_else(|| {
            anyhow!(
                "{} must define profile.plugin-release.opt-level = 3",
                cargo_toml.display()
            )
        })?;
    if opt_level != 3 {
        bail!(
            "{} must set profile.plugin-release.opt-level = 3",
            cargo_toml.display()
        );
    }

    let lto = profile
        .get("lto")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            anyhow!(
                "{} must define profile.plugin-release.lto = \"fat\"",
                cargo_toml.display()
            )
        })?;
    if lto != "fat" {
        bail!(
            "{} must set profile.plugin-release.lto = \"fat\"",
            cargo_toml.display()
        );
    }

    let strip = profile
        .get("strip")
        .and_then(|value| value.as_bool())
        .ok_or_else(|| {
            anyhow!(
                "{} must define profile.plugin-release.strip = true",
                cargo_toml.display()
            )
        })?;
    if !strip {
        bail!(
            "{} must set profile.plugin-release.strip = true",
            cargo_toml.display()
        );
    }

    let panic = profile
        .get("panic")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            anyhow!(
                "{} must define profile.plugin-release.panic = \"abort\"",
                cargo_toml.display()
            )
        })?;
    if panic != "abort" {
        bail!(
            "{} must set profile.plugin-release.panic = \"abort\"",
            cargo_toml.display()
        );
    }

    Ok(())
}

fn configured_rustup_toolchain(ctx: &TaskContext) -> Result<Option<RustupToolchain>> {
    let Some(rustup) = rustup_binary() else {
        return Ok(None);
    };

    if let Some(toolchain) = rustup_toolchain_override() {
        return Ok(Some(RustupToolchain { rustup, toolchain }));
    }

    if let Some(toolchain) = rustup_toolchain_from_file(&ctx.path("rust-toolchain.toml"))? {
        return Ok(Some(RustupToolchain { rustup, toolchain }));
    }

    let mut active = Command::new(&rustup);
    active.current_dir(&ctx.repo_root);
    active.args(["show", "active-toolchain"]);
    Ok(run_capture(&mut active)?
        .split_whitespace()
        .next()
        .map(|toolchain| RustupToolchain {
            rustup,
            toolchain: toolchain.to_string(),
        }))
}

fn host_rust_has_wasm_target(ctx: &TaskContext) -> Result<bool> {
    let mut rustc = ctx.command("rustc");
    rustc.args(["--print", "target-libdir", "--target", WASM_TARGET]);
    Ok(rustc.output()?.status.success())
}

fn rustup_toolchain_has_target(
    _ctx: &TaskContext,
    rustup_toolchain: &RustupToolchain,
) -> Result<bool> {
    let mut targets = Command::new(&rustup_toolchain.rustup);
    targets.args([
        "target",
        "list",
        "--installed",
        "--toolchain",
        rustup_toolchain.toolchain.as_str(),
    ]);
    let installed_targets = run_capture(&mut targets)?;
    Ok(installed_targets
        .lines()
        .any(|line| line.trim() == WASM_TARGET))
}

fn rustup_toolchain_has_component(
    rustup_toolchain: &RustupToolchain,
    component: &str,
) -> Result<bool> {
    let mut components = Command::new(&rustup_toolchain.rustup);
    components.args([
        "component",
        "list",
        "--installed",
        "--toolchain",
        rustup_toolchain.toolchain.as_str(),
    ]);
    let installed_components = run_capture(&mut components)?;
    Ok(installed_components
        .lines()
        .any(|line| line.trim() == component))
}

fn ensure_rustup_component(rustup_toolchain: &RustupToolchain, component: &str) -> Result<()> {
    if rustup_toolchain_has_component(rustup_toolchain, component)? {
        return Ok(());
    }

    step(format!(
        "Installing {component} for rustup toolchain {}",
        rustup_toolchain.toolchain
    ));
    let mut command = Command::new(&rustup_toolchain.rustup);
    command.args([
        "component",
        "add",
        "--toolchain",
        rustup_toolchain.toolchain.as_str(),
        component,
    ]);
    run_checked(&mut command).with_context(|| {
        format!(
            "failed to install {component} for rustup toolchain {}",
            rustup_toolchain.toolchain
        )
    })
}

fn ensure_rustup_wasm_target(ctx: &TaskContext, rustup_toolchain: &RustupToolchain) -> Result<()> {
    if rustup_toolchain_has_target(ctx, rustup_toolchain)? {
        return Ok(());
    }

    step(format!(
        "Installing {WASM_TARGET} for rustup toolchain {}",
        rustup_toolchain.toolchain
    ));
    let mut command = Command::new(&rustup_toolchain.rustup);
    command.args([
        "target",
        "add",
        "--toolchain",
        rustup_toolchain.toolchain.as_str(),
        WASM_TARGET,
    ]);
    run_checked(&mut command).with_context(|| {
        format!(
            "failed to install {WASM_TARGET} for rustup toolchain {}",
            rustup_toolchain.toolchain
        )
    })
}

fn rustup_which(rustup_toolchain: &RustupToolchain, binary: &str) -> Result<PathBuf> {
    let mut command = Command::new(&rustup_toolchain.rustup);
    command.args([
        "which",
        binary,
        "--toolchain",
        rustup_toolchain.toolchain.as_str(),
    ]);
    let path = run_capture(&mut command)?;
    Ok(PathBuf::from(path.trim()))
}

fn rustup_cargo_command_in(rustup_toolchain: &RustupToolchain, cwd: &Path) -> Result<Command> {
    let cargo = rustup_which(rustup_toolchain, "cargo")?;
    let rustc = rustup_which(rustup_toolchain, "rustc")?;
    let rustdoc = rustup_which(rustup_toolchain, "rustdoc").ok();

    let mut command = Command::new(&cargo);
    command.current_dir(cwd);
    command.env("CARGO", &cargo);
    command.env("RUSTC", &rustc);
    command.env("RUSTUP_TOOLCHAIN", rustup_toolchain.toolchain.as_str());
    // Preserve outer wrappers like sccache for rustup-pinned cargo invocations.
    command.env_remove("RUSTC_WORKSPACE_WRAPPER");
    if let Some(rustdoc) = rustdoc {
        command.env("RUSTDOC", rustdoc);
    }
    if let Some(toolchain_bin) = cargo.parent() {
        let existing_path = env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![toolchain_bin.to_path_buf()];
        paths.extend(env::split_paths(&existing_path));
        let joined = env::join_paths(paths).context("join rustup PATH")?;
        command.env("PATH", joined);
    }
    apply_local_sdk_override(&mut command)?;
    Ok(command)
}

fn local_sdk_override_path() -> Result<Option<PathBuf>> {
    let Some(raw) = env::var_os(SDK_LOCAL_OVERRIDE_ENV) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }

    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .context("resolve current working directory for local SDK override")?
            .join(path)
    };
    let canonical = absolute.canonicalize().with_context(|| {
        format!(
            "{SDK_LOCAL_OVERRIDE_ENV} points to missing or unreadable path '{}'",
            absolute.display()
        )
    })?;
    Ok(Some(canonical))
}

fn apply_local_sdk_override(command: &mut Command) -> Result<()> {
    let Some(path) = local_sdk_override_path()? else {
        return Ok(());
    };

    command.args([
        "--config",
        &format!(
            "patch.crates-io.scryer-plugin-sdk.path=\"{}\"",
            path.display()
        ),
    ]);
    Ok(())
}

fn repo_cargo_command_in(ctx: &TaskContext, cwd: &Path) -> Result<Command> {
    if let Some(rustup_toolchain) = configured_rustup_toolchain(ctx)? {
        return rustup_cargo_command_in(&rustup_toolchain, cwd);
    }

    let mut command = ctx.command_in("cargo", cwd);
    apply_local_sdk_override(&mut command)?;
    Ok(command)
}

fn ci_target_dir(ctx: &TaskContext, cwd: &Path) -> Result<PathBuf> {
    let toolchain = configured_rustup_toolchain(ctx)?
        .map(|toolchain| {
            toolchain
                .toolchain
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                .collect::<String>()
        })
        .unwrap_or_else(|| "host".to_string());
    Ok(cwd.join("target").join("ci").join(toolchain))
}

fn ci_cargo_command_in(ctx: &TaskContext, cwd: &Path) -> Result<Command> {
    let mut command = repo_cargo_command_in(ctx, cwd)?;
    command.env("CARGO_TARGET_DIR", ci_target_dir(ctx, cwd)?);
    Ok(command)
}

fn cargo_target_dir(cwd: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join("target"))
}

fn wasm_build_command_in(ctx: &TaskContext, cwd: &Path) -> Result<Command> {
    if let Some(rustup_toolchain) = configured_rustup_toolchain(ctx)? {
        ensure_rustup_wasm_target(ctx, &rustup_toolchain)?;
        return rustup_cargo_command_in(&rustup_toolchain, cwd);
    }

    if host_rust_has_wasm_target(ctx)? {
        return Ok(ctx.command_in("cargo", cwd));
    }

    bail!(
        "{WASM_TARGET} target is unavailable. Install rustup so xtask can bootstrap the repo toolchain, or add {WASM_TARGET} to the active host Rust toolchain."
    )
}

fn require_wasm_target(ctx: &TaskContext) -> Result<()> {
    if let Some(rustup_toolchain) = configured_rustup_toolchain(ctx)? {
        ensure_rustup_wasm_target(ctx, &rustup_toolchain)?;
        return Ok(());
    }

    if host_rust_has_wasm_target(ctx)? {
        return Ok(());
    }

    bail!(
        "{WASM_TARGET} target is unavailable. Install rustup so xtask can bootstrap the repo toolchain, or add {WASM_TARGET} to the active host Rust toolchain."
    )
}

fn git_capture(ctx: &TaskContext, args: &[&str]) -> Result<String> {
    let mut command = ctx.command_in("git", &ctx.repo_root);
    command.args(args);
    run_capture(&mut command)
}

fn git_has_cached_changes(ctx: &TaskContext) -> Result<bool> {
    let mut command = ctx.command_in("git", &ctx.repo_root);
    command.args(["diff", "--cached", "--quiet"]);
    let status = run_status(&mut command)?;
    Ok(!status.success())
}

fn current_branch(ctx: &TaskContext) -> Result<String> {
    git_capture(ctx, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|value| value.trim().to_string())
}

fn prompt_continue_if_dirty(ctx: &TaskContext) -> Result<()> {
    let status = git_capture(ctx, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok(());
    }
    warn("Working tree has uncommitted changes:");
    for line in status.lines() {
        eprintln!("     {line}");
    }
    eprint!("\n   Continue anyway? [y/N] ");
    io::stderr().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    if !matches!(response.trim(), "y" | "Y") {
        bail!("aborted");
    }
    Ok(())
}

fn parse_bump(args: &ReleaseOptions) -> Result<(VersionBump, Option<Version>)> {
    let explicit = match &args.version {
        Some(version) => Some(Version::parse(version.trim_start_matches('v'))?),
        None => None,
    };
    let bump = if args.major {
        VersionBump::Major
    } else if args.minor {
        VersionBump::Minor
    } else {
        VersionBump::Patch
    };
    Ok((bump, explicit))
}

fn release_options_from_plan_args(args: &OfficialPlanChangedArgs) -> ReleaseOptions {
    ReleaseOptions {
        major: args.major,
        minor: args.minor,
        patch: args.patch,
        dry_run: false,
        version: args.version.clone(),
    }
}

fn next_version(current: &Version, bump: VersionBump) -> Version {
    let mut next = current.clone();
    match bump {
        VersionBump::Patch => next.patch += 1,
        VersionBump::Minor => {
            next.minor += 1;
            next.patch = 0;
        }
        VersionBump::Major => {
            next.major += 1;
            next.minor = 0;
            next.patch = 0;
        }
    }
    next.pre = Default::default();
    next.build = Default::default();
    next
}

fn crate_name_from_manifest(path: &Path) -> Result<String> {
    let document = fs::read_to_string(path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    document["package"]["name"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("package.name missing from {}", path.display()))
}

fn version_from_manifest(path: &Path) -> Result<Version> {
    let document = fs::read_to_string(path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let version = document["package"]["version"]
        .as_str()
        .ok_or_else(|| anyhow!("package.version missing from {}", path.display()))?;
    Ok(Version::parse(version)?)
}

fn write_manifest_version(path: &Path, version: &Version) -> Result<()> {
    let mut document = fs::read_to_string(path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    document["package"]["version"] = value(version.to_string());
    fs::write(path, document.to_string())?;
    Ok(())
}

fn git_checkout_paths(ctx: &TaskContext, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut command = ctx.command_in("git", &ctx.repo_root);
    command.arg("checkout").arg("--");
    for path in paths {
        let relative = path
            .strip_prefix(&ctx.repo_root)
            .unwrap_or(path.as_path())
            .to_path_buf();
        command.arg(relative);
    }
    run_checked(&mut command)
}

fn git_path_is_tracked(ctx: &TaskContext, path: &Path) -> Result<bool> {
    let relative = path
        .strip_prefix(&ctx.repo_root)
        .unwrap_or(path)
        .to_path_buf();
    let mut command = ctx.command_in("git", &ctx.repo_root);
    command.args(["ls-files", "--error-unmatch"]);
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.arg(relative);
    Ok(run_status(&mut command)?.success())
}

fn plugin_inventory_roots() -> [&'static str; 4] {
    ["indexers", "download_clients", "notifications", "subtitles"]
}

fn read_manifest_document(path: &Path) -> Result<DocumentMut> {
    fs::read_to_string(path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn plugin_manifest_metadata(manifest_path: &Path) -> Result<PluginManifestMetadata> {
    let document = read_manifest_document(manifest_path)?;
    let scryer_metadata = document
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("scryer"));
    let description = document
        .get("package")
        .and_then(|package| package.get("description"))
        .and_then(|description| description.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let official = scryer_metadata
        .and_then(|scryer| scryer.get("official"))
        .and_then(|official| official.as_bool())
        .ok_or_else(|| {
            anyhow!(
                "{} must define package.metadata.scryer.official as true or false",
                manifest_path.display()
            )
        })?;
    let plugin_id = scryer_metadata
        .and_then(|scryer| scryer.get("plugin_id"))
        .and_then(|plugin_id| plugin_id.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let status = match scryer_metadata
        .and_then(|scryer| scryer.get("status"))
        .and_then(|status| status.as_str())
    {
        Some(value) => PluginCatalogStatus::parse(value)?,
        None => PluginCatalogStatus::Active,
    };
    let catalog_versions = match scryer_metadata
        .and_then(|scryer| scryer.get("catalog_versions"))
        .and_then(|catalog_versions| catalog_versions.as_array())
    {
        Some(values) => {
            let parsed = values
                .iter()
                .map(|value| {
                    let value = value.as_str().ok_or_else(|| {
                        anyhow!(
                            "{} package.metadata.scryer.catalog_versions entries must be strings",
                            manifest_path.display()
                        )
                    })?;
                    CatalogVersion::parse(value)
                })
                .collect::<Result<BTreeSet<_>>>()?;
            if parsed.is_empty() {
                bail!(
                    "{} must define at least one package.metadata.scryer.catalog_versions entry",
                    manifest_path.display()
                );
            }
            parsed
        }
        None => default_catalog_versions(),
    };
    let feature_sets = parse_feature_sets(manifest_path, scryer_metadata)?;
    let min_scryer_version = scryer_metadata
        .and_then(|scryer| scryer.get("min_scryer_version"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(value) = min_scryer_version.as_deref() {
        Version::parse(value).with_context(|| {
            format!(
                "{} package.metadata.scryer.min_scryer_version must be a valid semver version",
                manifest_path.display()
            )
        })?;
    }
    let docs_url = scryer_metadata
        .and_then(|scryer| scryer.get("docs_url"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let source_repo = scryer_metadata
        .and_then(|scryer| scryer.get("source_repo"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let distribution_base_url = scryer_metadata
        .and_then(|scryer| scryer.get("distribution_base_url"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    if official {
        if description.is_empty() {
            bail!(
                "{} must define a non-empty package.description for official plugins",
                manifest_path.display()
            );
        }
        if plugin_id.is_none() {
            bail!(
                "{} must define a non-empty package.metadata.scryer.plugin_id for official plugins",
                manifest_path.display()
            );
        }
        if catalog_versions.contains(&CatalogVersion::V2)
            && !feature_sets_include_baseline(&feature_sets)
        {
            bail!(
                "{} official plugins publishing catalog-v2 must include a baseline package.metadata.scryer.feature_sets entry with required_features = []",
                manifest_path.display()
            );
        }
    }

    Ok(PluginManifestMetadata {
        description,
        official,
        plugin_id,
        status,
        catalog_versions,
        feature_sets,
        min_scryer_version,
        docs_url,
        source_repo,
        distribution_base_url,
    })
}

fn tracked_plugin_crate_dirs(ctx: &TaskContext) -> Result<Vec<PathBuf>> {
    let mut plugin_dirs = Vec::new();
    for prefix in plugin_inventory_roots() {
        let prefix_dir = ctx.path(prefix);
        if !prefix_dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&prefix_dir)
            .with_context(|| format!("failed to read {}", prefix_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let manifest_path = path.join("Cargo.toml");
            if path.is_dir()
                && manifest_path.is_file()
                && git_path_is_tracked(ctx, &manifest_path)?
                && is_plugin_crate(&manifest_path)?
            {
                plugin_dirs.push(path);
            }
        }
    }
    plugin_dirs.sort();
    Ok(plugin_dirs)
}

fn official_plugin_dirs_by_id(ctx: &TaskContext) -> Result<BTreeMap<String, PathBuf>> {
    let mut dirs = BTreeMap::new();
    for plugin_dir in tracked_plugin_crate_dirs(ctx)? {
        let manifest_path = plugin_dir.join("Cargo.toml");
        let metadata = plugin_manifest_metadata(&manifest_path)?;
        if !metadata.official {
            continue;
        }

        let plugin_id = metadata
            .plugin_id
            .expect("official plugin manifest metadata should already be validated");
        if let Some(existing) = dirs.insert(plugin_id.clone(), plugin_dir.clone()) {
            bail!(
                "duplicate official plugin id '{}' in {} and {}",
                plugin_id,
                existing.display(),
                plugin_dir.display()
            );
        }
    }
    Ok(dirs)
}

fn discover_local_official_plugin(ctx: &TaskContext, plugin_id: &str) -> Result<LocalPluginInfo> {
    let plugin_dirs = official_plugin_dirs_by_id(ctx)?;
    let plugin_dir = plugin_dirs
        .get(plugin_id)
        .ok_or_else(|| anyhow!("plugin '{}' not found in local official plugins", plugin_id))?;
    discover_local_plugin(ctx, plugin_dir)
}

fn official_plugin_catalog_versions(
    ctx: &TaskContext,
    plugin_id: &str,
) -> Result<BTreeSet<CatalogVersion>> {
    let plugin_dirs = official_plugin_dirs_by_id(ctx)?;
    let plugin_dir = plugin_dirs
        .get(plugin_id)
        .ok_or_else(|| anyhow!("plugin '{}' not found in local official plugins", plugin_id))?;
    Ok(plugin_manifest_metadata(&plugin_dir.join("Cargo.toml"))?.catalog_versions)
}

fn local_plugin_directories(ctx: &TaskContext) -> Result<Vec<PathBuf>> {
    let mut plugin_dirs = Vec::new();
    for plugin_dir in tracked_plugin_crate_dirs(ctx)? {
        let manifest_path = plugin_dir.join("Cargo.toml");
        if plugin_manifest_metadata(&manifest_path)?.official {
            plugin_dirs.push(plugin_dir);
        }
    }
    plugin_dirs.sort();
    Ok(plugin_dirs)
}

fn package_version(manifest_path: &Path) -> Result<String> {
    let document = read_manifest_document(manifest_path)?;
    let version = document["package"]["version"]
        .as_str()
        .ok_or_else(|| anyhow!("{} must define package.version", manifest_path.display()))?;
    Ok(version.trim().to_string())
}

fn plugin_crate_version(plugin_dir: &Path) -> Result<String> {
    package_version(&plugin_dir.join("Cargo.toml"))
}

fn discover_local_plugin(ctx: &TaskContext, plugin_dir: &Path) -> Result<LocalPluginInfo> {
    let cargo_toml = plugin_dir.join("Cargo.toml");
    let crate_name = crate_name_from_manifest(&cargo_toml)?;
    let current_version = version_from_manifest(&cargo_toml)?;
    let manifest_metadata = plugin_manifest_metadata(&cargo_toml)?;
    let description = manifest_metadata.description.clone();
    let manifest_plugin_id = manifest_metadata.plugin_id.as_deref().ok_or_else(|| {
        anyhow!(
            "{} is missing package.metadata.scryer.plugin_id",
            cargo_toml.display()
        )
    })?;
    let plugin_repo_path = path_relative_to_repo(ctx, plugin_dir)?;
    let default_repo_url =
        format!("https://github.com/scryer-media/scryer-plugins/tree/main/{plugin_repo_path}");
    let docs_url = manifest_metadata
        .docs_url
        .clone()
        .unwrap_or_else(|| default_repo_url.clone());
    let source_repo = manifest_metadata
        .source_repo
        .clone()
        .unwrap_or(default_repo_url);
    let distribution_base_url = manifest_metadata
        .distribution_base_url
        .clone()
        .unwrap_or_else(|| format!("{}/plugins/{manifest_plugin_id}", public_catalog_base_url()));
    let descriptor_feature_set = primary_feature_set(&manifest_metadata.feature_sets);
    let wasm = build_plugin_wasm(ctx, plugin_dir, descriptor_feature_set)?;
    let descriptor = load_descriptor_from_wasm(&wasm)?;
    validate_descriptor_contract(&descriptor)?;
    if descriptor.id != manifest_plugin_id {
        bail!(
            "{} package.metadata.scryer.plugin_id '{}' does not match descriptor id '{}'",
            cargo_toml.display(),
            manifest_plugin_id,
            descriptor.id
        );
    }

    Ok(LocalPluginInfo {
        plugin_id: descriptor.id.clone(),
        name: descriptor.name.clone(),
        description,
        plugin_type: descriptor.plugin_type().to_string(),
        provider_type: descriptor.provider_type().to_string(),
        status: manifest_metadata.status,
        catalog_versions: manifest_metadata.catalog_versions,
        feature_sets: manifest_metadata.feature_sets,
        min_scryer_version: manifest_metadata.min_scryer_version,
        docs_url,
        plugin_dir: plugin_dir.to_path_buf(),
        cargo_toml,
        crate_name,
        current_version,
        source_repo,
        distribution_base_url,
    })
}

fn discover_local_plugins(ctx: &TaskContext) -> Result<Vec<LocalPluginInfo>> {
    local_plugin_directories(ctx)?
        .into_iter()
        .map(|plugin_dir| discover_local_plugin(ctx, &plugin_dir))
        .collect()
}

fn plugin_publishes_catalog(plugin: &LocalPluginInfo, version: CatalogVersion) -> bool {
    plugin.catalog_versions.contains(&version)
}

fn catalog_v2_base_sdk_version() -> Version {
    Version::parse(CATALOG_V2_BASE_SDK_VERSION).expect("catalog-v2 base sdk must be valid semver")
}

fn catalog_v2_minimum_sdk_version(sdk_constraint: &str) -> Result<Option<Version>> {
    let requirement = semver::VersionReq::parse(sdk_constraint)
        .with_context(|| format!("invalid SDK constraint {sdk_constraint}"))?;
    let minimum = requirement
        .comparators
        .iter()
        .filter(|comparator| {
            matches!(
                comparator.op,
                semver::Op::Exact
                    | semver::Op::Greater
                    | semver::Op::GreaterEq
                    | semver::Op::Tilde
                    | semver::Op::Caret
                    | semver::Op::Wildcard
            )
        })
        .map(|comparator| Version {
            major: comparator.major,
            minor: comparator.minor.unwrap_or(0),
            patch: comparator.patch.unwrap_or(0),
            pre: comparator.pre.clone(),
            build: Default::default(),
        })
        .min();
    Ok(minimum)
}

fn catalog_v2_supported_sdk_constraint(sdk_constraint: &str) -> Result<bool> {
    let Some(minimum) = catalog_v2_minimum_sdk_version(sdk_constraint)? else {
        return Ok(false);
    };
    Ok(minimum >= catalog_v2_base_sdk_version())
}

fn catalog_v2_supported_child_releases(
    releases: Vec<ChildCatalogReleaseV2>,
) -> Result<Vec<ChildCatalogReleaseV2>> {
    let mut filtered = Vec::new();
    for release in releases {
        let matches =
            catalog_v2_supported_sdk_constraint(&release.sdk_constraint).with_context(|| {
                format!(
                    "{}: invalid SDK constraint {}",
                    release.version, release.sdk_constraint
                )
            })?;
        if matches {
            filtered.push(release);
        }
    }
    Ok(filtered)
}

fn read_child_catalog_releases_from_path(
    ctx: &TaskContext,
    path: &Path,
) -> Result<Vec<ChildCatalogReleaseV2>> {
    let bytes = read_catalog_bytes(ctx, path)?;
    let catalog: ChildCatalogV2 = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse child catalog {}", path.display()))?;
    catalog_v2_supported_child_releases(catalog.releases)
}

fn read_published_child_catalog_releases(
    ctx: &TaskContext,
    plugin_id: &str,
) -> Result<Vec<ChildCatalogReleaseV2>> {
    let catalog = read_published_official_catalog(ctx)?;
    let Some(entry) = catalog.plugins.iter().find(|plugin| plugin.id == plugin_id) else {
        return Ok(Vec::new());
    };
    let temp = tempfile::tempdir()?;
    let (tag, asset) = release_asset_url_parts(&entry.child_catalog_url, OFFICIAL_GITHUB_REPO)?;
    let child_path = github_release_download(ctx, OFFICIAL_GITHUB_REPO, &tag, &asset, temp.path())?;
    read_child_catalog_releases_from_path(ctx, &child_path)
}

fn resolve_release_target_for_plugin(
    ctx: &TaskContext,
    plugin: &LocalPluginInfo,
    options: &ReleaseOptions,
) -> Result<ReleaseTarget> {
    let existing_releases = read_published_child_catalog_releases(ctx, &plugin.plugin_id)?;
    let has_existing_release = !existing_releases.is_empty()
        || latest_plugin_release_tag(ctx, &plugin.plugin_id)?.is_some();
    let (bump, explicit) = parse_bump(options)?;
    let next_version = match explicit {
        Some(version) => version,
        None if has_existing_release => next_version(&plugin.current_version, bump),
        None => plugin.current_version.clone(),
    };
    let next_version_text = next_version.to_string();
    if existing_releases
        .iter()
        .any(|release| release.version == next_version_text)
    {
        bail!(
            "Plugin '{}' already has a {} release in published child catalog history",
            plugin.plugin_id,
            next_version
        );
    }

    let tag_name = official_plugin_release_tag(&plugin.plugin_id, &next_version.to_string());

    Ok(ReleaseTarget {
        plugin_id: plugin.plugin_id.clone(),
        plugin_dir: plugin.plugin_dir.clone(),
        cargo_toml: plugin.cargo_toml.clone(),
        crate_name: plugin.crate_name.clone(),
        current_version: plugin.current_version.clone(),
        next_version,
        tag_name,
    })
}

fn resolve_release_target(
    ctx: &TaskContext,
    plugins: &[LocalPluginInfo],
    plugin_name: &str,
    options: &ReleaseOptions,
) -> Result<ReleaseTarget> {
    let plugin = plugins
        .iter()
        .find(|plugin| plugin.plugin_id == plugin_name)
        .ok_or_else(|| {
            anyhow!(
                "Plugin '{}' not found in local official plugins",
                plugin_name
            )
        })?;
    resolve_release_target_for_plugin(ctx, plugin, options)
}

fn release_commit_message(targets: &[ReleaseTarget]) -> String {
    if targets.len() == 1 {
        return format!(
            "release: {} {}",
            targets[0].plugin_id, targets[0].next_version
        );
    }
    if targets.len() <= 3 {
        let summary = targets
            .iter()
            .map(|target| format!("{} {}", target.plugin_id, target.next_version))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("release: {summary}");
    }
    format!("release: plugin batch ({})", targets.len())
}

fn release_tag_prefix(plugin_id: &str) -> String {
    format!("plugins/{plugin_id}/v")
}

fn repo_release_tag_prefix() -> &'static str {
    REPO_RELEASE_TAG_PREFIX
}

fn legacy_release_tag_prefix(plugin_id: &str) -> String {
    format!("{plugin_id}-v")
}

fn release_tag_v3_prefix(plugin_id: &str) -> String {
    format!("plugins-v3/{plugin_id}/v")
}

fn release_tag_version(plugin_id: &str, tag: &str) -> Option<Version> {
    tag.strip_prefix(&release_tag_prefix(plugin_id))
        .or_else(|| tag.strip_prefix(&release_tag_v3_prefix(plugin_id)))
        .or_else(|| tag.strip_prefix(&legacy_release_tag_prefix(plugin_id)))
        .and_then(|version| Version::parse(version).ok())
}

fn latest_plugin_release_tag(ctx: &TaskContext, plugin_id: &str) -> Result<Option<String>> {
    let tags = git_capture(ctx, &["tag", "--merged", "HEAD"])?;
    Ok(tags
        .lines()
        .filter_map(|tag| release_tag_version(plugin_id, tag).map(|version| (version, tag)))
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, tag)| tag.to_string()))
}

fn head_short_sha(ctx: &TaskContext) -> Result<String> {
    git_capture(ctx, &["rev-parse", "--short=12", "HEAD"]).map(|value| value.trim().to_string())
}

fn repo_release_tag_name(ctx: &TaskContext) -> Result<String> {
    let unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_secs();
    Ok(format!(
        "{}{}-{}",
        repo_release_tag_prefix(),
        unix_seconds,
        head_short_sha(ctx)?
    ))
}

fn path_relative_to_repo(ctx: &TaskContext, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(&ctx.repo_root)
        .with_context(|| {
            format!(
                "{} is not inside {}",
                path.display(),
                ctx.repo_root.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn changed_paths_since(ctx: &TaskContext, tag: &str) -> Result<BTreeSet<String>> {
    Ok(
        git_capture(ctx, &["diff", "--name-only", &format!("{tag}..HEAD")])?
            .lines()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    )
}

fn artifact_wide_change_reason(path: &str) -> Option<&'static str> {
    match path {
        "rust-toolchain.toml" => Some("Rust toolchain changed"),
        "xtask/Cargo.toml" | "xtask/Cargo.lock" => Some("release tooling dependencies changed"),
        ".cargo/config.toml" => Some("cargo build configuration changed"),
        _ => None,
    }
}

fn path_is_under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn release_impact_for_plugin(ctx: &TaskContext, plugin: &LocalPluginInfo) -> Result<ReleaseImpact> {
    let Some(tag) = latest_plugin_release_tag(ctx, &plugin.plugin_id)? else {
        return Ok(ReleaseImpact::PluginChanged);
    };
    let changed = changed_paths_since(ctx, &tag)?;
    if let Some(reason) = changed
        .iter()
        .find_map(|path| artifact_wide_change_reason(path))
    {
        return Ok(ReleaseImpact::ArtifactWide(reason.to_string()));
    }

    let plugin_dir = path_relative_to_repo(ctx, &plugin.plugin_dir)?;
    if changed.iter().any(|path| path_is_under(path, &plugin_dir)) {
        return Ok(ReleaseImpact::PluginChanged);
    }

    Ok(ReleaseImpact::Unchanged)
}

fn run_release_changed(ctx: &TaskContext, args: ReleaseChangedArgs) -> Result<()> {
    let plans = collect_changed_release_targets(ctx, &args.options)?;
    if plans.is_empty() {
        ok("No official plugin changes detected since per-plugin release tags");
        return Ok(());
    }

    step("Selected changed official plugins");
    for plan in &plans {
        println!("   {}: {}", plan.target.plugin_id, plan.reason);
    }

    let targets = plans.into_iter().map(|plan| plan.target).collect();
    run_tag_only_release_targets(ctx, targets, &args.options)
}

fn collect_changed_release_targets(
    ctx: &TaskContext,
    options: &ReleaseOptions,
) -> Result<Vec<PlannedReleaseTarget>> {
    ensure_current_sdk_dependency_is_published(ctx)?;
    let plugins = discover_local_plugins(ctx)?;
    let mut selected = Vec::new();
    for plugin in &plugins {
        match release_impact_for_plugin(ctx, plugin)? {
            ReleaseImpact::PluginChanged => {
                selected.push((
                    plugin.plugin_id.clone(),
                    "plugin-specific changes".to_string(),
                ));
            }
            ReleaseImpact::ArtifactWide(reason) => {
                selected.push((plugin.plugin_id.clone(), reason));
            }
            ReleaseImpact::Unchanged => {}
        }
    }

    selected.sort_by(|left, right| left.0.cmp(&right.0));
    selected.dedup_by(|left, right| left.0 == right.0);
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    if options.version.is_some() && selected.len() != 1 {
        bail!("--version can only be used when exactly one changed plugin is selected");
    }

    let mut targets = Vec::new();
    for (plugin_id, reason) in selected {
        targets.push(PlannedReleaseTarget {
            target: resolve_release_target(ctx, &plugins, &plugin_id, options)?,
            reason,
        });
    }
    Ok(targets)
}

fn run_tag_only_release_targets(
    ctx: &TaskContext,
    targets: Vec<ReleaseTarget>,
    options: &ReleaseOptions,
) -> Result<()> {
    step("Determining next versions");
    for target in &targets {
        println!("   Plugin ID  : {}", target.plugin_id);
        println!("   Plugin dir : {}", target.plugin_dir.display());
        println!("   Crate name : {}", target.crate_name);
        println!("   Current    : {}", target.current_version);
        println!("   Next       : {}", target.next_version);
        println!("   Tag        : {}", target.tag_name);
    }
    if options.dry_run {
        println!("   {YELLOW}(dry run — no commits, tags, or pushes){RESET}");
    }

    step("Pre-flight checks");
    let tags = git_capture(ctx, &["tag"])?;
    for target in &targets {
        if tags.lines().any(|line| line == target.tag_name) {
            bail!("Tag {} already exists", target.tag_name);
        }
    }
    let branch = current_branch(ctx)?;
    println!("   Branch: {branch}");
    prompt_continue_if_dirty(ctx)?;
    require_wasm_target(ctx)?;
    run_ci_strict(
        ctx,
        &CiScopeArgs {
            plugin_ids: targets
                .iter()
                .map(|target| target.plugin_id.clone())
                .collect(),
        },
    )?;
    ok("Pre-flight OK");

    let lockfiles = targets
        .iter()
        .map(|target| target.plugin_dir.join("Cargo.lock"))
        .collect::<Vec<_>>();
    let lockfile_tracked_before = lockfiles
        .iter()
        .map(|lockfile| git_path_is_tracked(ctx, lockfile))
        .collect::<Result<Vec<_>>>()?;

    for target in &targets {
        step(format!(
            "Bumping {} to {}",
            target.crate_name, target.next_version
        ));
        write_manifest_version(&target.cargo_toml, &target.next_version)?;
        refresh_lockfile(ctx, &target.plugin_dir)?;
        ok(format!("{} Cargo.toml updated", target.crate_name));
    }

    for target in &targets {
        step(format!(
            "Building {} (release, wasm32-wasip1)",
            target.crate_name
        ));
        let manifest_metadata = plugin_manifest_metadata(&target.cargo_toml)?;
        let mut descriptor = None;
        let mut descriptor_json = None;
        for feature_set in &manifest_metadata.feature_sets {
            let built_wasm = build_plugin_wasm(ctx, &target.plugin_dir, feature_set)?;
            let current_descriptor = load_descriptor_from_wasm(&built_wasm)?;
            validate_descriptor_contract(&current_descriptor)?;
            let current_descriptor_json = serde_json::to_string(&current_descriptor)?;
            if let Some(expected_json) = &descriptor_json {
                if expected_json != &current_descriptor_json {
                    bail!(
                        "{}: descriptor differs across feature_sets after release bump",
                        target.plugin_id
                    );
                }
            } else {
                descriptor_json = Some(current_descriptor_json);
                descriptor = Some(current_descriptor);
            }
        }
        ok("Built release WASM variants");

        step(format!("Validating {}", target.plugin_id));
        let descriptor = descriptor.expect("feature_sets should never be empty");
        let descriptor_version = Version::parse(&descriptor.version).with_context(|| {
            format!(
                "{}: descriptor version {} is not valid semver",
                descriptor.id, descriptor.version
            )
        })?;
        if descriptor.id != target.plugin_id {
            bail!(
                "built descriptor id {} does not match plugin id {}",
                descriptor.id,
                target.plugin_id
            );
        }
        if descriptor_version != target.next_version {
            bail!(
                "{}: built descriptor version {} does not match requested release version {}",
                descriptor.id,
                descriptor.version,
                target.next_version
            );
        }
        ok(format!(
            "Validated descriptor {} {} ({})",
            descriptor.id,
            descriptor.version,
            descriptor.plugin_type()
        ));
    }

    if options.dry_run {
        println!("\n{YELLOW}{BOLD}Dry run complete — stopping before commit/tag/push.{RESET}");
        let mut restore = targets
            .iter()
            .map(|target| target.cargo_toml.clone())
            .collect::<Vec<_>>();
        restore.extend(
            lockfiles
                .iter()
                .zip(lockfile_tracked_before.iter())
                .filter_map(|(lockfile, tracked_before)| {
                    tracked_before.then_some(lockfile.clone())
                }),
        );
        if !restore.is_empty() {
            git_checkout_paths(ctx, &restore)?;
        }
        for (lockfile, tracked_before) in lockfiles.iter().zip(lockfile_tracked_before.iter()) {
            if !tracked_before && lockfile.exists() {
                let _ = fs::remove_file(lockfile);
            }
        }
        return Ok(());
    }

    step("Committing changes");
    let mut add = ctx.command_in("git", &ctx.repo_root);
    add.arg("add");
    for target in &targets {
        add.arg(&target.cargo_toml);
    }
    for lockfile in &lockfiles {
        if lockfile.exists() {
            add.arg(lockfile);
        }
    }
    run_checked(&mut add)?;
    if git_has_cached_changes(ctx)? {
        let mut commit = ctx.command_in("git", &ctx.repo_root);
        let commit_message = release_commit_message(&targets);
        commit.args(["commit", "-m", &commit_message]);
        run_checked(&mut commit)?;
        ok("Committed");
    } else {
        ok("No release-prep file changes to commit; tagging current HEAD");
    }

    for target in &targets {
        step(format!("Creating signed tag {}", target.tag_name));
        let mut tag = ctx.command_in("git", &ctx.repo_root);
        tag.args([
            "tag",
            "-s",
            &target.tag_name,
            "-m",
            &format!("Release {}", target.tag_name),
        ]);
        run_checked(&mut tag)?;
        ok(format!("Tag {} created", target.tag_name));
    }

    let release_tag = repo_release_tag_name(ctx)?;
    step(format!("Creating signed release trigger tag {release_tag}"));
    let mut release_tag_command = ctx.command_in("git", &ctx.repo_root);
    release_tag_command.args([
        "tag",
        "-s",
        &release_tag,
        "-m",
        &format!("Release trigger for {}", release_commit_message(&targets)),
    ]);
    run_checked(&mut release_tag_command)?;
    ok(format!("Tag {release_tag} created"));

    step("Pushing to origin");
    let mut push_branch = ctx.command_in("git", &ctx.repo_root);
    push_branch.args(["push", "origin", &branch]);
    run_checked(&mut push_branch)?;
    let mut push_tags = ctx.command_in("git", &ctx.repo_root);
    push_tags.arg("push").arg("origin");
    for target in &targets {
        push_tags.arg(&target.tag_name);
    }
    run_checked(&mut push_tags)?;
    let mut push_release_tag = ctx.command_in("git", &ctx.repo_root);
    push_release_tag.args(["push", "origin", &release_tag]);
    run_checked(&mut push_release_tag)?;
    ok(format!(
        "Pushed {}, {} plugin tag(s), and {}",
        branch,
        targets.len(),
        release_tag
    ));

    println!(
        "\n{GREEN}{BOLD}Released {} plugin tag(s) without touching legacy plugin inventory metadata{RESET}",
        targets.len()
    );
    println!("   Release batch tag: {release_tag}");
    Ok(())
}

fn run_release_targets(
    ctx: &TaskContext,
    targets: Vec<ReleaseTarget>,
    options: &ReleaseOptions,
) -> Result<()> {
    run_tag_only_release_targets(ctx, targets, options)
}

fn wasm_filename_for_manifest(cargo_toml: &Path) -> Result<String> {
    Ok(crate_name_from_manifest(cargo_toml)?.replace('-', "_") + ".wasm")
}

fn ensure_lockfile(ctx: &TaskContext, plugin_dir: &Path) -> Result<()> {
    let lockfile = plugin_dir.join("Cargo.lock");
    if lockfile.is_file() {
        return Ok(());
    }

    step(format!("Generating lockfile for {}", plugin_dir.display()));
    let mut command = repo_cargo_command_in(ctx, plugin_dir)?;
    command.args(["generate-lockfile", "--offline"]);
    run_checked(&mut command)
        .with_context(|| format!("failed to generate lockfile for {}", plugin_dir.display()))
}

fn refresh_lockfile(ctx: &TaskContext, plugin_dir: &Path) -> Result<()> {
    step(format!("Refreshing lockfile for {}", plugin_dir.display()));
    let mut command = repo_cargo_command_in(ctx, plugin_dir)?;
    command.args(["generate-lockfile", "--offline"]);
    run_checked(&mut command)
        .with_context(|| format!("failed to refresh lockfile for {}", plugin_dir.display()))
}

fn prefetch_plugin_dependencies(ctx: &TaskContext, plugin_dir: &Path) -> Result<()> {
    let lockfile = plugin_dir.join("Cargo.lock");
    if !lockfile.is_file() {
        bail!(
            "missing lockfile for {}; run cargo xtask release-changed locally before publishing",
            plugin_dir.display()
        );
    }

    step(format!(
        "Prefetching dependencies for {}",
        plugin_dir.display()
    ));
    let mut host_command = repo_cargo_command_in(ctx, plugin_dir)?;
    host_command.args(["fetch", "--locked"]);
    run_checked(&mut host_command).with_context(|| {
        format!(
            "failed to prefetch host dependencies for {}",
            plugin_dir.display()
        )
    })?;

    let mut target_command = repo_cargo_command_in(ctx, plugin_dir)?;
    target_command.args(["fetch", "--locked", "--target", WASM_TARGET]);
    run_checked(&mut target_command).with_context(|| {
        format!(
            "failed to prefetch {WASM_TARGET} dependencies for {}",
            plugin_dir.display()
        )
    })
}

fn wasm_variant_target_dir(plugin_dir: &Path, feature_set: &WasmFeatureSet) -> PathBuf {
    cargo_target_dir(plugin_dir)
        .join("variants")
        .join(feature_set.target_dir_component())
}

fn append_rustflags(command: &mut Command, flag: &str) {
    let existing = env::var("RUSTFLAGS").unwrap_or_default();
    let combined = if existing.trim().is_empty() {
        flag.to_string()
    } else {
        format!("{existing} {flag}")
    };
    command.env("RUSTFLAGS", combined);
}

fn build_plugin_wasm(
    ctx: &TaskContext,
    plugin_dir: &Path,
    feature_set: &WasmFeatureSet,
) -> Result<PathBuf> {
    let cargo_toml = plugin_dir.join("Cargo.toml");
    validate_plugin_release_profile(&cargo_toml)?;
    let wasm_filename = wasm_filename_for_manifest(&cargo_toml)?;

    step(format!("Building {}", plugin_dir.display()));
    ensure_lockfile(ctx, plugin_dir)?;
    let mut build = wasm_build_command_in(ctx, plugin_dir)?;
    build.env(
        "CARGO_TARGET_DIR",
        wasm_variant_target_dir(plugin_dir, feature_set),
    );
    build.env(
        "SCRYER_WASM_REQUIRED_FEATURES",
        feature_set.required_features_env_value(),
    );
    if let Some(rust_target_feature_flag) = feature_set.rust_target_feature_flag() {
        append_rustflags(&mut build, &rust_target_feature_flag);
    }
    build.args([
        "build",
        "--profile",
        "plugin-release",
        "--target",
        WASM_TARGET,
        "--locked",
        "--offline",
    ]);
    run_checked(&mut build)?;

    let built_wasm = wasm_variant_target_dir(plugin_dir, feature_set)
        .join(WASM_TARGET)
        .join("plugin-release")
        .join(wasm_filename);
    if !built_wasm.is_file() {
        bail!("expected WASM at {} but not found", built_wasm.display());
    }
    Ok(built_wasm)
}

fn required_exports_for_descriptor(descriptor: &PluginDescriptor) -> Vec<&'static str> {
    let mut exports = vec![EXPORT_DESCRIBE];
    match &descriptor.provider {
        ProviderDescriptor::Indexer(_) => exports.push(EXPORT_INDEXER_SEARCH),
        ProviderDescriptor::DownloadClient(_) => exports.extend([
            EXPORT_DOWNLOAD_ADD,
            EXPORT_DOWNLOAD_LIST_QUEUE,
            EXPORT_DOWNLOAD_LIST_HISTORY,
            EXPORT_DOWNLOAD_LIST_COMPLETED,
            EXPORT_DOWNLOAD_CONTROL,
            EXPORT_DOWNLOAD_MARK_IMPORTED,
            EXPORT_DOWNLOAD_STATUS,
            EXPORT_DOWNLOAD_TEST_CONNECTION,
        ]),
        ProviderDescriptor::Notification(_) => exports.push(EXPORT_NOTIFICATION_SEND),
        ProviderDescriptor::Subtitle(subtitle) => {
            exports.push(EXPORT_VALIDATE_CONFIG);
            match subtitle.capabilities.mode {
                SubtitleProviderMode::Catalog => {
                    exports.extend([EXPORT_SUBTITLE_SEARCH, EXPORT_SUBTITLE_DOWNLOAD]);
                }
                SubtitleProviderMode::Generator => exports.push(EXPORT_SUBTITLE_GENERATE),
            }
        }
    }
    exports
}

fn instantiate_plugin_from_wasm(
    wasm_path: &Path,
    timeout: std::time::Duration,
) -> Result<extism::Plugin> {
    let bytes =
        fs::read(wasm_path).with_context(|| format!("failed to read {}", wasm_path.display()))?;
    let manifest = Manifest::new([extism::Wasm::data(bytes)]).with_timeout(timeout);
    let socket_stubs = UserData::new(());
    extism::PluginBuilder::new(manifest)
        .with_wasi(true)
        .with_function_in_namespace(
            "extism:host/user",
            "scryer_socket_open",
            [ValType::I64],
            [ValType::I64],
            socket_stubs.clone(),
            socket_unsupported,
        )
        .with_function_in_namespace(
            "extism:host/user",
            "scryer_socket_read",
            [ValType::I64],
            [ValType::I64],
            socket_stubs.clone(),
            socket_unsupported,
        )
        .with_function_in_namespace(
            "extism:host/user",
            "scryer_socket_write",
            [ValType::I64],
            [ValType::I64],
            socket_stubs.clone(),
            socket_unsupported,
        )
        .with_function_in_namespace(
            "extism:host/user",
            "scryer_socket_starttls",
            [ValType::I64],
            [ValType::I64],
            socket_stubs.clone(),
            socket_unsupported,
        )
        .with_function_in_namespace(
            "extism:host/user",
            "scryer_socket_close",
            [ValType::I64],
            [ValType::I64],
            socket_stubs,
            socket_unsupported,
        )
        .build()
        .with_context(|| format!("failed to instantiate {}", wasm_path.display()))
}

fn load_descriptor_from_wasm(wasm_path: &Path) -> Result<PluginDescriptor> {
    let mut plugin = instantiate_plugin_from_wasm(wasm_path, std::time::Duration::from_secs(10))?;

    if !plugin.function_exists(EXPORT_DESCRIBE) {
        bail!("plugin is missing required export {EXPORT_DESCRIBE}");
    }

    let output: String = plugin
        .call::<&str, String>(EXPORT_DESCRIBE, "")
        .with_context(|| format!("{EXPORT_DESCRIBE} failed"))?;
    let descriptor: PluginDescriptor =
        serde_json::from_str(&output).context("scryer_describe returned invalid JSON")?;

    let missing = required_exports_for_descriptor(&descriptor)
        .into_iter()
        .filter(|export| !plugin.function_exists(export))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "{} ({}) is missing required export(s): {}",
            descriptor.id,
            descriptor.plugin_type(),
            missing.join(", ")
        );
    }

    Ok(descriptor)
}

fn validate_descriptor_contract(descriptor: &PluginDescriptor) -> Result<()> {
    validate_sdk_contract(
        &descriptor.id,
        &descriptor.sdk_version,
        &descriptor.sdk_constraint,
        &descriptor.sdk_version,
    )
    .map_err(anyhow::Error::msg)?;
    if descriptor.id.trim().is_empty() {
        bail!("descriptor id must not be empty");
    }
    if descriptor.provider_type().trim().is_empty() {
        bail!("{}: provider_type must not be empty", descriptor.id);
    }
    validate_plugin_descriptor_host_permissions(descriptor).map_err(anyhow::Error::msg)?;
    Ok(())
}

fn validate_subtitle_sync_variant_parity(
    plugin_dir: &Path,
    variants: &[BuiltPluginVariant],
) -> Result<()> {
    let Some(baseline) = variants
        .iter()
        .find(|variant| variant.feature_set.is_baseline())
    else {
        return Ok(());
    };
    let optimized_variants = variants
        .iter()
        .filter(|variant| !variant.feature_set.is_baseline())
        .collect::<Vec<_>>();
    if optimized_variants.is_empty() {
        return Ok(());
    }

    for optimized in optimized_variants {
        for subtitle_format in SUBTITLE_SYNC_PARITY_FORMATS {
            let request = subtitle_sync_parity_request(plugin_dir, subtitle_format)?;
            let baseline_response = call_subtitle_sync_align(&baseline.wasm_path, &request)
                .with_context(|| {
                    format!("baseline subtitle-sync parity run failed for {subtitle_format}")
                })?;
            let optimized_response = call_subtitle_sync_align(&optimized.wasm_path, &request)
                .with_context(|| {
                    format!(
                        "{} subtitle-sync parity run failed for {subtitle_format}",
                        optimized.feature_set.slug()
                    )
                })?;
            assert_subtitle_sync_parity(
                subtitle_format,
                &optimized.feature_set,
                &baseline_response,
                &optimized_response,
            )?;
        }
    }

    ok(format!(
        "subtitle-sync scalar/SIMD parity validated across {} subtitle format(s)",
        SUBTITLE_SYNC_PARITY_FORMATS.len()
    ));
    Ok(())
}

fn subtitle_sync_parity_request(plugin_dir: &Path, subtitle_format: &str) -> Result<String> {
    let fixture_root = plugin_dir.join("tests/fixtures/test-data");
    let subtitle_path = fixture_root
        .join("subtitles")
        .join(subtitle_format)
        .join(format!("late_1750.{subtitle_format}"));
    let reference_path = fixture_root
        .join("subtitles")
        .join(subtitle_format)
        .join(format!("aligned.{subtitle_format}"));
    let subtitle_content = fs::read(&subtitle_path)
        .with_context(|| format!("failed to read {}", subtitle_path.display()))?;
    let reference_content = fs::read(&reference_path)
        .with_context(|| format!("failed to read {}", reference_path.display()))?;

    let request = SubtitleSyncAlignRequest {
        input: scryer_plugin_sdk::SubtitleSyncAlignInputRef {
            path: fixture_root.join("media/missing-reference.mp4"),
        },
        subtitle: SubtitleSyncInputSubtitle {
            content_base64: BASE64.encode(subtitle_content),
            format: subtitle_format.to_string(),
            file_name: subtitle_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string),
            encoding_hint: Some("utf-8".to_string()),
        },
        reference_subtitle: Some(SubtitleSyncReferenceSubtitle {
            content_base64: BASE64.encode(reference_content),
            format: subtitle_format.to_string(),
            file_name: reference_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string),
            encoding_hint: Some("utf-8".to_string()),
        }),
        subtitle_spans: Vec::new(),
        max_offset_seconds: 8,
        sync_options: Some(scryer_plugin_sdk::SubtitleSyncOptions::default()),
        selector: Some(scryer_plugin_sdk::AudioStreamSelector::Default),
        expected_codec: None,
    };
    serde_json::to_string(&request).context("failed to encode subtitle-sync parity request")
}

fn call_subtitle_sync_align(
    wasm_path: &Path,
    request_json: &str,
) -> Result<SubtitleSyncAlignResponse> {
    let mut plugin = instantiate_plugin_from_wasm(wasm_path, std::time::Duration::from_secs(30))?;
    if !plugin.function_exists(EXPORT_SUBSYNC_ALIGN) {
        bail!("plugin is missing required export {EXPORT_SUBSYNC_ALIGN}");
    }

    let output: String = plugin
        .call::<&str, String>(EXPORT_SUBSYNC_ALIGN, request_json)
        .with_context(|| format!("{EXPORT_SUBSYNC_ALIGN} failed"))?;
    match serde_json::from_str::<PluginResult<SubtitleSyncAlignResponse>>(&output)
        .context("scryer_subsync_align returned invalid JSON")?
    {
        PluginResult::Ok(response) => Ok(response),
        PluginResult::Err(error) => bail!(
            "scryer_subsync_align returned plugin error: {}",
            error.public_message
        ),
    }
}

fn assert_subtitle_sync_parity(
    subtitle_format: &str,
    optimized_feature_set: &WasmFeatureSet,
    baseline: &SubtitleSyncAlignResponse,
    optimized: &SubtitleSyncAlignResponse,
) -> Result<()> {
    let context = || {
        format!(
            "subtitle-sync scalar/SIMD parity mismatch for {subtitle_format} against {}",
            optimized_feature_set.slug()
        )
    };

    if baseline.applied != optimized.applied {
        bail!("{}: applied differs", context());
    }
    if baseline.offset_ms != optimized.offset_ms {
        bail!(
            "{}: offset_ms differs (baseline {}, optimized {})",
            context(),
            baseline.offset_ms,
            optimized.offset_ms
        );
    }
    if baseline.skipped_reason != optimized.skipped_reason {
        bail!(
            "{}: skipped_reason differs (baseline {:?}, optimized {:?})",
            context(),
            baseline.skipped_reason,
            optimized.skipped_reason
        );
    }
    if baseline
        .rewritten_subtitle
        .as_ref()
        .map(|rewritten| (rewritten.format.as_str(), rewritten.content_base64.as_str()))
        != optimized
            .rewritten_subtitle
            .as_ref()
            .map(|rewritten| (rewritten.format.as_str(), rewritten.content_base64.as_str()))
    {
        bail!("{}: rewritten subtitle bytes differ", context());
    }
    if baseline.warnings != optimized.warnings {
        bail!(
            "{}: warnings differ (baseline {:?}, optimized {:?})",
            context(),
            baseline.warnings,
            optimized.warnings
        );
    }
    if baseline.message != optimized.message {
        bail!(
            "{}: message differs (baseline {:?}, optimized {:?})",
            context(),
            baseline.message,
            optimized.message
        );
    }

    compare_optional_float(context(), "score", baseline.score, optimized.score)?;
    compare_optional_float(
        context(),
        "selected_framerate_ratio",
        baseline.selected_framerate_ratio,
        optimized.selected_framerate_ratio,
    )?;
    compare_optional_float(
        context(),
        "consistency_ratio",
        baseline.consistency_ratio,
        optimized.consistency_ratio,
    )?;
    compare_optional_float(
        context(),
        "nosplit_score",
        baseline.nosplit_score,
        optimized.nosplit_score,
    )?;
    compare_optional_float(
        context(),
        "split_score",
        baseline.split_score,
        optimized.split_score,
    )?;

    Ok(())
}

fn compare_optional_float(
    context: String,
    field: &str,
    baseline: Option<f64>,
    optimized: Option<f64>,
) -> Result<()> {
    match (baseline, optimized) {
        (Some(left), Some(right)) if (left - right).abs() <= SUBTITLE_SYNC_FLOAT_TOLERANCE => {
            Ok(())
        }
        (None, None) => Ok(()),
        _ => bail!("{context}: {field} differs (baseline {baseline:?}, optimized {optimized:?})"),
    }
}

fn run_plugin_validate(ctx: &TaskContext, args: PluginValidateArgs) -> Result<()> {
    let plugin_dir = if args.path.is_file() {
        args.path
            .parent()
            .ok_or_else(|| anyhow!("invalid plugin path {}", args.path.display()))?
            .to_path_buf()
    } else {
        args.path
    };
    let plugin_dir = if plugin_dir.is_absolute() {
        plugin_dir
    } else {
        ctx.repo_root.join(plugin_dir)
    };
    if !plugin_dir.join("Cargo.toml").is_file() {
        bail!("{} does not contain Cargo.toml", plugin_dir.display());
    }

    let manifest_metadata = plugin_manifest_metadata(&plugin_dir.join("Cargo.toml"))?;
    let mut descriptor = None;
    let mut descriptor_json = None;
    let mut built_variants = Vec::new();
    for feature_set in &manifest_metadata.feature_sets {
        let wasm_path = build_plugin_wasm(ctx, &plugin_dir, feature_set)?;
        let current_descriptor = load_descriptor_from_wasm(&wasm_path)?;
        validate_descriptor_contract(&current_descriptor)?;
        let current_descriptor_json = serde_json::to_string(&current_descriptor)?;
        if let Some(expected_json) = &descriptor_json {
            if expected_json != &current_descriptor_json {
                bail!(
                    "{}: descriptor differs across feature_sets; baseline and optimized variants must expose the same contract",
                    plugin_dir.display()
                );
            }
        } else {
            descriptor_json = Some(current_descriptor_json);
            descriptor = Some(current_descriptor);
        }
        built_variants.push(BuiltPluginVariant {
            feature_set: feature_set.clone(),
            wasm_path,
        });
    }
    let descriptor = descriptor.expect("feature_sets should never be empty");
    if descriptor.id == ENHANCED_SUBTITLE_SYNC_PLUGIN_ID {
        validate_subtitle_sync_variant_parity(&plugin_dir, &built_variants)?;
    }
    ok(format!(
        "Validated {} {} ({}) across {} feature set(s)",
        descriptor.id,
        descriptor.version,
        descriptor.plugin_type(),
        manifest_metadata.feature_sets.len()
    ));
    Ok(())
}

fn run_doctor(ctx: &TaskContext) -> Result<()> {
    step("Checking plugin maintainer toolchain");
    for (tool, args) in [
        ("git", ["--version"].as_slice()),
        ("cargo", ["--version"].as_slice()),
        ("wasm-opt", ["--version"].as_slice()),
        ("zstd", ["--version"].as_slice()),
        ("cosign", ["version"].as_slice()),
        ("gh", ["--version"].as_slice()),
    ] {
        match ctx.command(tool).args(args).status() {
            Ok(status) if status.success() => ok(format!("{tool} available")),
            _ => warn(format!("{tool} unavailable or not healthy")),
        }
    }
    if let Some(path) = local_sdk_override_path()? {
        ok(format!(
            "temporary local scryer-plugin-sdk override active ({})",
            path.display()
        ));
    }
    require_wasm_target(ctx)?;
    match current_sdk_dependency(ctx) {
        Ok(SdkDependency::Published(version)) => {
            match ensure_published_sdk_version(ctx, &version) {
                Ok(()) => ok("published scryer-plugin-sdk dependency is available"),
                Err(error) => warn(error.to_string()),
            }
        }
        Ok(SdkDependency::GitTag { tag, version }) => {
            ok(format!(
                "temporary git-sourced scryer-plugin-sdk dependency active ({tag} -> {version})"
            ));
        }
        Err(error) => warn(error.to_string()),
    }
    ok(format!(
        "release artifacts use wasm-opt {WASM_OPT_LEVEL_SIZE}/{WASM_OPT_LEVEL_SPEED} and zstd {ZSTD_LEVEL}"
    ));
    Ok(())
}

fn is_plugin_crate(manifest_path: &Path) -> Result<bool> {
    let document = fs::read_to_string(manifest_path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let Some(crate_types) = document
        .get("lib")
        .and_then(|lib| lib.get("crate-type"))
        .and_then(|crate_type| crate_type.as_array())
    else {
        return Ok(false);
    };

    Ok(crate_types
        .iter()
        .any(|crate_type| crate_type.as_str() == Some("cdylib")))
}

fn plugin_crate_dirs(ctx: &TaskContext) -> Result<Vec<PathBuf>> {
    tracked_plugin_crate_dirs(ctx)
}

fn ci_project_dirs(ctx: &TaskContext) -> Result<Vec<PathBuf>> {
    let mut dirs = plugin_crate_dirs(ctx)?;
    dirs.push(ctx.repo_root.join("xtask"));
    dirs.sort();
    Ok(dirs)
}

fn scoped_ci_project_dirs(ctx: &TaskContext, scope: &CiScopeArgs) -> Result<Vec<PathBuf>> {
    if scope.plugin_ids.is_empty() {
        return ci_project_dirs(ctx);
    }

    let plugin_dirs = official_plugin_dirs_by_id(ctx)?;
    let mut dirs = BTreeSet::new();
    let mut selected = BTreeSet::new();
    for plugin_id in &scope.plugin_ids {
        if !selected.insert(plugin_id.clone()) {
            continue;
        }

        let plugin_dir = plugin_dirs
            .get(plugin_id)
            .ok_or_else(|| anyhow!("plugin '{plugin_id}' not found in local official plugins"))?;
        dirs.insert(plugin_dir.clone());
    }

    Ok(dirs.into_iter().collect())
}

fn ensure_cargo_audit(ctx: &TaskContext) -> Result<()> {
    let mut version = repo_cargo_command_in(ctx, &ctx.repo_root)?;
    version.args(["audit", "--version"]);
    if run_status(&mut version)
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return Ok(());
    }

    step("Installing cargo-audit");
    let mut install = repo_cargo_command_in(ctx, &ctx.repo_root)?;
    install.args(["install", "--locked", "cargo-audit"]);
    run_checked(&mut install)?;
    ok("cargo-audit installed");
    Ok(())
}

fn run_ci_fmt_check(ctx: &TaskContext, scope: &CiScopeArgs) -> Result<()> {
    if scope.plugin_ids.is_empty() {
        step("Checking cargo fmt across plugin crates and xtask");
    } else {
        step("Checking cargo fmt for selected plugin crates and xtask");
    }
    if let Some(rustup_toolchain) = configured_rustup_toolchain(ctx)? {
        ensure_rustup_component(&rustup_toolchain, "rustfmt")?;
    }
    for project_dir in scoped_ci_project_dirs(ctx, scope)? {
        let relative = path_relative_to_repo(ctx, &project_dir)?;
        println!("   cargo fmt --check :: {relative}");
        let mut fmt = repo_cargo_command_in(ctx, &project_dir)?;
        fmt.args(["fmt", "--check"]);
        run_checked(&mut fmt)?;
    }
    ok("cargo fmt passed");
    Ok(())
}

fn run_ci_strict_clippy(ctx: &TaskContext, scope: &CiScopeArgs) -> Result<()> {
    if scope.plugin_ids.is_empty() {
        step("Running strict clippy across plugin crates and xtask");
    } else {
        step("Running strict clippy for selected plugin crates and xtask");
    }
    if let Some(rustup_toolchain) = configured_rustup_toolchain(ctx)? {
        ensure_rustup_component(&rustup_toolchain, "clippy")?;
    }
    for project_dir in scoped_ci_project_dirs(ctx, scope)? {
        let relative = path_relative_to_repo(ctx, &project_dir)?;
        println!("   cargo clippy -D warnings :: {relative}");
        let mut clippy = ci_cargo_command_in(ctx, &project_dir)?;
        clippy.args([
            "clippy",
            "--all-targets",
            "--all-features",
            "--locked",
            "--",
        ]);
        clippy.args(["-D", "warnings"]);
        run_checked(&mut clippy)?;
    }
    ok("strict clippy passed");
    Ok(())
}

fn run_ci_audit(ctx: &TaskContext, scope: &CiScopeArgs) -> Result<()> {
    if scope.plugin_ids.is_empty() {
        step("Running cargo audit across plugin crates and xtask");
    } else {
        step("Running cargo audit for selected plugin crates and xtask");
    }
    ensure_cargo_audit(ctx)?;
    warn(format!(
        "Ignoring advisories pending upstream runtime fixes: {}",
        AUDIT_IGNORE_ADVISORIES.join(" ")
    ));
    for project_dir in scoped_ci_project_dirs(ctx, scope)? {
        let relative = path_relative_to_repo(ctx, &project_dir)?;
        println!("   cargo audit :: {relative}");
        let mut audit = repo_cargo_command_in(ctx, &project_dir)?;
        audit.args(["audit", "--file", "Cargo.lock"]);
        for advisory in AUDIT_IGNORE_ADVISORIES {
            audit.args(["--ignore", advisory]);
        }
        run_checked(&mut audit)?;
    }
    ok("cargo audit passed");
    Ok(())
}

fn run_ci_strict(ctx: &TaskContext, scope: &CiScopeArgs) -> Result<()> {
    run_ci_fmt_check(ctx, scope)?;
    run_ci_audit(ctx, scope)?;
    run_ci_strict_clippy(ctx, scope)?;
    Ok(())
}

fn run_plugin_build_all(ctx: &TaskContext) -> Result<()> {
    step("Building all plugin crates");
    ensure_current_sdk_dependency_is_published(ctx)?;
    require_wasm_target(ctx)?;
    for dir in plugin_crate_dirs(ctx)? {
        let manifest_metadata = plugin_manifest_metadata(&dir.join("Cargo.toml"))?;
        for feature_set in &manifest_metadata.feature_sets {
            build_plugin_wasm(ctx, &dir, feature_set)?;
        }
    }
    ok("all plugin crates built");
    Ok(())
}

fn run_plugin_validate_all(ctx: &TaskContext) -> Result<()> {
    step("Validating all plugin crates");
    ensure_current_sdk_dependency_is_published(ctx)?;
    for dir in plugin_crate_dirs(ctx)? {
        run_plugin_validate(ctx, PluginValidateArgs { path: dir })?;
    }
    ok("all plugin descriptors validated");
    Ok(())
}

fn run_sdk_bump(ctx: &TaskContext, version: &str) -> Result<()> {
    step(format!("Bumping scryer-plugin-sdk dependency to {version}"));
    Version::parse(version).with_context(|| format!("invalid SDK version {version}"))?;
    ensure_published_sdk_version(ctx, version)?;
    for dir in plugin_crate_dirs(ctx)? {
        let manifest_path = dir.join("Cargo.toml");
        let mut doc = fs::read_to_string(&manifest_path)?
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        doc["dependencies"]["scryer-plugin-sdk"] = value(version);
        fs::write(&manifest_path, doc.to_string())?;
        ensure_lockfile(ctx, &dir)?;
        refresh_lockfile(ctx, &dir)?;
    }
    ok("SDK dependencies bumped");
    Ok(())
}

enum SdkDependency {
    Published(String),
    GitTag { tag: String, version: String },
}

fn current_sdk_dependency(ctx: &TaskContext) -> Result<SdkDependency> {
    let manifest_path = ctx.repo_root.join("xtask/Cargo.toml");
    let document = fs::read_to_string(&manifest_path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let dependency = &document["dependencies"]["scryer-plugin-sdk"];
    if let Some(version) = dependency.as_str() {
        return Ok(SdkDependency::Published(version.trim().to_string()));
    }
    let git = dependency["git"].as_str();
    let tag = dependency["tag"].as_str();
    match (git, tag) {
        (Some(_), Some(tag)) => {
            let version = tag
                .trim()
                .strip_prefix("plugin-sdk-v")
                .ok_or_else(|| {
                    anyhow!(
                        "xtask/Cargo.toml temporary scryer-plugin-sdk git dependency must use a plugin-sdk-v<semver> tag"
                    )
                })?
                .to_string();
            Version::parse(&version)
                .with_context(|| format!("invalid SDK version derived from git tag {tag}"))?;
            Ok(SdkDependency::GitTag {
                tag: tag.trim().to_string(),
                version,
            })
        }
        _ => Err(anyhow!(
            "xtask/Cargo.toml must depend on scryer-plugin-sdk by version or plugin-sdk-v<semver> git tag"
        )),
    }
}

fn ensure_current_sdk_dependency_is_published(ctx: &TaskContext) -> Result<()> {
    if local_sdk_override_path()?.is_some() {
        return Ok(());
    }

    match current_sdk_dependency(ctx)? {
        SdkDependency::Published(version) => ensure_published_sdk_version(ctx, &version),
        SdkDependency::GitTag { .. } => Ok(()),
    }
}

fn ensure_published_sdk_version(ctx: &TaskContext, version: &str) -> Result<()> {
    let package = format!("scryer-plugin-sdk@{version}");
    let mut command = ctx.command("cargo");
    command.args(["info", &package]);
    if run_status(&mut command)?.success() {
        Ok(())
    } else {
        bail!("{package} is not published on crates.io yet; publish the SDK before bumping plugins")
    }
}

fn blake3_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
}

fn shake256_file(path: &Path) -> Result<String> {
    use sha3::{
        Shake256,
        digest::{ExtendableOutput, Update, XofReader},
    };

    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Shake256::default();
    hasher.update(&bytes);
    let mut reader = hasher.finalize_xof();
    let mut output = [0_u8; 32];
    XofReader::read(&mut reader, &mut output);
    Ok(format!(
        "shake256:{}",
        output
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn file_digests(path: &Path) -> Result<Vec<String>> {
    Ok(vec![blake3_file(path)?, shake256_file(path)?])
}

fn digest_value<'a>(digests: &'a [String], algorithm: &str) -> Result<&'a str> {
    digests
        .iter()
        .find_map(|digest| digest.strip_prefix(&format!("{algorithm}:")))
        .ok_or_else(|| anyhow!("missing {algorithm} digest"))
}

fn hashed_filename(logical_name: &str, digests: &[String]) -> Result<String> {
    let digest = digest_value(digests, "blake3")?;
    if let Some((prefix, suffix)) = logical_name.split_once('.') {
        Ok(format!("{prefix}.{digest}.{suffix}"))
    } else {
        Ok(format!("{logical_name}-{digest}"))
    }
}

fn versioned_hashed_filename(
    logical_name: &str,
    version: u64,
    digests: &[String],
) -> Result<String> {
    let digest = digest_value(digests, "blake3")?;
    let short_digest = digest.get(..SHORT_CATALOG_HASH_LEN).ok_or_else(|| {
        anyhow!("blake3 digest is shorter than {SHORT_CATALOG_HASH_LEN} hex chars")
    })?;
    if let Some((prefix, suffix)) = logical_name.split_once('.') {
        Ok(format!("{prefix}.{version}.{short_digest}.{suffix}"))
    } else {
        Ok(format!("{logical_name}.{version}.{short_digest}"))
    }
}

fn trim_url_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn env_override_or_default(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn versioned_distribution_url(base: &str, version: &str, file_name: &str) -> String {
    format!("{}/v{version}/{file_name}", trim_url_base(base))
}

fn signature_bundle_file_name(file_name: &str) -> String {
    format!("{file_name}.bundle.zst")
}

fn redirect_signature_bundle_file_name(file_name: &str) -> String {
    if let Some(prefix) = file_name.strip_suffix(".json") {
        return format!("{prefix}.bundle.json");
    }
    format!("{file_name}.bundle.json")
}

fn official_release_workflow() -> String {
    env_override_or_default(
        OFFICIAL_RELEASE_WORKFLOW_ENV,
        DEFAULT_OFFICIAL_RELEASE_WORKFLOW,
    )
}

fn official_plugin_release_tag_prefix() -> String {
    env_override_or_default(
        OFFICIAL_PLUGIN_RELEASE_TAG_PREFIX_ENV,
        DEFAULT_OFFICIAL_PLUGIN_RELEASE_TAG_PREFIX,
    )
}

fn central_catalog_v3_release_tag() -> String {
    env_override_or_default(
        CENTRAL_CATALOG_V3_RELEASE_TAG_ENV,
        DEFAULT_CENTRAL_CATALOG_V3_RELEASE_TAG,
    )
}

fn central_catalog_v3_path_prefix() -> String {
    env_override_or_default(
        CENTRAL_CATALOG_V3_PATH_PREFIX_ENV,
        DEFAULT_CENTRAL_CATALOG_V3_PATH_PREFIX,
    )
}

fn public_catalog_base_url() -> String {
    env::var(R2_PUBLIC_BASE_URL_ENV)
        .ok()
        .map(|value| trim_url_base(&value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_R2_PUBLIC_BASE_URL.to_string())
}

fn url_file_name(url: &str) -> Result<String> {
    let normalized = url.trim().trim_end_matches('/');
    normalized
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("failed to determine file name from URL {url}"))
}

fn url_path_key(url: &str) -> Result<String> {
    let Some((_, remainder)) = url.split_once("://") else {
        bail!("invalid URL {url}");
    };
    let Some((_, path)) = remainder.split_once('/') else {
        bail!("URL {url} does not include an object path");
    };
    if path.is_empty() {
        bail!("URL {url} does not include an object path");
    }
    Ok(path.to_string())
}

fn content_type_for_upload(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if name.ends_with(".json") {
        "application/json"
    } else if name.ends_with(".zst") {
        "application/zstd"
    } else if name.ends_with(".br") {
        "application/brotli"
    } else {
        "application/octet-stream"
    }
}

fn validate_digest_string(label: &str, digest: &str) -> Result<()> {
    let Some((algorithm, hex)) = digest.split_once(':') else {
        bail!("{label} must be formatted as <algorithm>:<hex>");
    };
    let expected_hex_len = match algorithm {
        "blake3" | "shake256" => 64,
        _ => bail!("{label} uses unsupported digest algorithm {algorithm}"),
    };
    if hex.len() != expected_hex_len || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        bail!(
            "{label} must contain a {expected_hex_len}-character lowercase hex digest for {algorithm}"
        );
    }
    Ok(())
}

fn write_brotli_file(input: &Path, output: &Path) -> Result<()> {
    let bytes = fs::read(input).with_context(|| format!("failed to read {}", input.display()))?;
    let mut compressed = Vec::new();
    {
        let mut writer =
            brotli::CompressorWriter::new(&mut compressed, 4096, BROTLI_QUALITY, BROTLI_LGWIN);
        writer.write_all(&bytes)?;
    }
    fs::write(output, compressed).with_context(|| format!("failed to write {}", output.display()))
}

fn read_brotli_file(path: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut reader = brotli::Decompressor::new(bytes.as_slice(), 4096);
    let mut decompressed = Vec::new();
    reader.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

fn decompress_plugin_wasm_artifact(
    ctx: &TaskContext,
    artifact: &Path,
    output: &Path,
) -> Result<()> {
    if artifact
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".zst"))
    {
        run_checked(
            ctx.command("zstd")
                .arg("-d")
                .arg("-f")
                .arg(artifact)
                .arg("-o")
                .arg(output),
        )?;
        return Ok(());
    }
    if artifact
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".br"))
    {
        fs::write(output, read_brotli_file(artifact)?)
            .with_context(|| format!("failed to write {}", output.display()))?;
        return Ok(());
    }
    if artifact
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".wasm"))
    {
        fs::copy(artifact, output)
            .with_context(|| format!("failed to copy {}", artifact.display()))?;
        return Ok(());
    }
    bail!(
        "unsupported plugin artifact encoding for {}",
        artifact.display()
    );
}

fn plugin_variant_artifact_stem(feature_set: &WasmFeatureSet, lane: PluginArtifactLane) -> String {
    match lane {
        PluginArtifactLane::V2 => feature_set.artifact_stem(),
        PluginArtifactLane::V3 => {
            if feature_set.is_baseline() {
                "plugin-v3".to_string()
            } else {
                format!("plugin-v3-{}", feature_set.slug())
            }
        }
    }
}

fn plugin_variant_uncompressed_file_name(
    feature_set: &WasmFeatureSet,
    lane: PluginArtifactLane,
) -> String {
    format!("{}.wasm", plugin_variant_artifact_stem(feature_set, lane))
}

fn plugin_variant_logical_file_name(
    feature_set: &WasmFeatureSet,
    lane: PluginArtifactLane,
    compression_suffix: &str,
) -> String {
    format!(
        "{}.wasm.{compression_suffix}",
        plugin_variant_artifact_stem(feature_set, lane)
    )
}

fn optimize_and_compress_wasm(
    ctx: &TaskContext,
    wasm: &Path,
    dist: &Path,
    feature_set: &WasmFeatureSet,
    lane: PluginArtifactLane,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    fs::create_dir_all(dist)?;
    let optimized = dist.join(plugin_variant_uncompressed_file_name(feature_set, lane));
    let compressed = dist.join(plugin_variant_logical_file_name(feature_set, lane, "zst"));
    let compressed_br = dist.join(plugin_variant_logical_file_name(feature_set, lane, "br"));
    let mut wasm_opt = ctx.command("wasm-opt");
    wasm_opt
        .arg(feature_set.wasm_opt_level())
        .arg("--enable-bulk-memory")
        .arg("--enable-sign-ext")
        .arg("--enable-nontrapping-float-to-int");
    for required_feature in &feature_set.required_features {
        wasm_opt.arg(required_feature.wasm_opt_flag());
    }
    wasm_opt.arg(wasm).arg("-o").arg(&optimized);
    run_checked(&mut wasm_opt)?;
    run_checked(
        ctx.command("zstd")
            .arg(ZSTD_LEVEL)
            .arg("-f")
            .arg(&optimized)
            .arg("-o")
            .arg(&compressed),
    )?;
    write_brotli_file(&optimized, &compressed_br)?;
    Ok((optimized, compressed, compressed_br))
}

fn github_release_asset_url(repo: &str, tag: &str, asset: &str) -> String {
    let tag = tag.replace('/', "%2F");
    format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
}

fn official_plugin_v3_distribution_base_url(plugin: &LocalPluginInfo) -> String {
    plugin
        .distribution_base_url
        .replace("/plugins/", "/plugins-v3/")
}

fn official_plugin_v3_github_mirror_urls(
    plugin_id: &str,
    version: &str,
    asset_name: &str,
) -> Vec<String> {
    vec![github_release_asset_url(
        OFFICIAL_GITHUB_REPO,
        &official_plugin_v3_release_tag(plugin_id, version),
        asset_name,
    )]
}

fn official_plugin_release_tag(plugin_id: &str, version: &str) -> String {
    format!(
        "{}/{plugin_id}/v{version}",
        official_plugin_release_tag_prefix()
    )
}

fn official_plugin_v3_release_tag(plugin_id: &str, version: &str) -> String {
    format!("plugins-v3/{plugin_id}/v{version}")
}

fn official_plugin_manifest_url(plugin_id: &str, version: &str) -> String {
    github_release_asset_url(
        OFFICIAL_GITHUB_REPO,
        &official_plugin_release_tag(plugin_id, version),
        "plugin.manifest.json",
    )
}

fn official_plugin_child_catalog_url(plugin_id: &str, version: &str) -> String {
    github_release_asset_url(
        OFFICIAL_GITHUB_REPO,
        &official_plugin_release_tag(plugin_id, version),
        CATALOG_MINIFIED_ZST,
    )
}

fn child_catalog_from_local_plugin(
    plugin: &LocalPluginInfo,
    releases: Vec<ChildCatalogReleaseV2>,
) -> Result<ChildCatalogV2> {
    let releases = merge_child_catalog_releases(&plugin.plugin_id, releases)?;

    Ok(ChildCatalogV2 {
        schema_version: CHILD_CATALOG_V2_SCHEMA.to_string(),
        id: plugin.plugin_id.clone(),
        name: plugin.name.clone(),
        description: plugin.description.clone(),
        plugin_type: plugin.plugin_type.clone(),
        provider_type: plugin.provider_type.clone(),
        publisher: "scryer".to_string(),
        support_tier: "official".to_string(),
        docs_url: plugin.docs_url.clone(),
        source_repo: plugin.source_repo.clone(),
        releases,
    })
}

fn default_child_catalog_dir(ctx: &TaskContext, plugin_id: &str) -> PathBuf {
    ctx.repo_root
        .join("dist")
        .join("catalog-v2")
        .join(plugin_id)
}

fn default_central_catalog_v3_dir(ctx: &TaskContext) -> PathBuf {
    ctx.repo_root.join("dist").join("catalog-v3")
}

fn catalog_asset_paths(dir: &Path) -> CatalogAssetPaths {
    CatalogAssetPaths {
        pretty_json: dir.join(CATALOG_PRETTY_JSON),
        minified_json: dir.join(CATALOG_MINIFIED_JSON),
        minified_zst: dir.join(CATALOG_MINIFIED_ZST),
    }
}

fn catalog_v3_asset_paths(dir: &Path) -> CatalogV3AssetPaths {
    CatalogV3AssetPaths {
        pretty_json: dir.join(CATALOG_V3_SNIPPET_JSON),
        minified_json: dir.join(CATALOG_V3_MINIFIED_JSON),
        minified_zst: dir.join(CATALOG_V3_MINIFIED_ZST),
        redirect_json: dir.join(CATALOG_V3_REDIRECT_JSON),
    }
}

fn catalog_v3_snippet_path(dir: &Path) -> PathBuf {
    dir.join(CATALOG_V3_SNIPPET_JSON)
}

fn write_catalog_assets<T: Serialize>(
    ctx: &TaskContext,
    catalog: &T,
    dir: &Path,
) -> Result<CatalogAssetPaths> {
    fs::create_dir_all(dir)?;
    let paths = catalog_asset_paths(dir);
    fs::write(
        &paths.pretty_json,
        serde_json::to_string_pretty(catalog)? + "\n",
    )?;
    fs::write(&paths.minified_json, serde_json::to_string(catalog)?)?;
    run_checked(
        ctx.command("zstd")
            .arg(ZSTD_LEVEL)
            .arg("-f")
            .arg(&paths.minified_json)
            .arg("-o")
            .arg(&paths.minified_zst),
    )?;
    Ok(paths)
}

fn write_catalog_v3_snippet(entry: &CatalogV3PluginEntry, dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = catalog_v3_snippet_path(dir);
    fs::write(&path, serde_json::to_string_pretty(entry)? + "\n")?;
    Ok(path)
}

fn write_catalog_v3_assets(
    ctx: &TaskContext,
    catalog: &CatalogV3,
    dir: &Path,
) -> Result<CatalogV3AssetPaths> {
    fs::create_dir_all(dir)?;
    let paths = catalog_v3_asset_paths(dir);
    fs::write(
        &paths.pretty_json,
        serde_json::to_string_pretty(catalog)? + "\n",
    )?;
    fs::write(&paths.minified_json, serde_json::to_string(catalog)?)?;
    run_checked(
        ctx.command("zstd")
            .arg(ZSTD_LEVEL)
            .arg("-f")
            .arg(&paths.minified_json)
            .arg("-o")
            .arg(&paths.minified_zst),
    )?;
    Ok(paths)
}

fn write_catalog_v3_redirect(locator: &CatalogV3Redirect, path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(locator)? + "\n")?;
    Ok(path.to_path_buf())
}

fn write_catalog_v3_redirects(
    redirect: &CatalogV3Redirect,
    paths: &CatalogV3AssetPaths,
) -> Result<PathBuf> {
    write_catalog_v3_redirect(redirect, &paths.redirect_json)
}

fn stage_hashed_central_catalog_v3_artifacts(
    paths: &CatalogV3AssetPaths,
    dir: &Path,
    catalog_version: u64,
) -> Result<Vec<(PathBuf, CatalogV3Artifact)>> {
    let base = format!(
        "{}/{}/{}",
        public_catalog_base_url(),
        central_catalog_v3_path_prefix(),
        catalog_version
    );
    let source = &paths.minified_zst;
    let logical_name = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("invalid central catalog asset {}", source.display()))?;
    let digests = file_digests(source)?;
    let file_name = versioned_hashed_filename(logical_name, catalog_version, &digests)?;
    fs::create_dir_all(dir)?;
    let hashed_path = dir.join(&file_name);
    fs::copy(source, &hashed_path).with_context(|| {
        format!(
            "failed to stage versioned hashed central catalog asset {}",
            hashed_path.display()
        )
    })?;
    let file_name = hashed_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("invalid staged catalog asset {}", hashed_path.display()))?
        .to_string();
    let signature_name = signature_bundle_file_name(&file_name);
    Ok(vec![(
        hashed_path,
        CatalogV3Artifact {
            url: format!("{base}/{file_name}"),
            mirror_urls: vec![github_release_asset_url(
                OFFICIAL_GITHUB_REPO,
                &central_catalog_v3_release_tag(),
                &file_name,
            )],
            signature_url: format!("{base}/{signature_name}"),
            signature_mirror_urls: vec![github_release_asset_url(
                OFFICIAL_GITHUB_REPO,
                &central_catalog_v3_release_tag(),
                &signature_name,
            )],
            digests,
        },
    )])
}

fn read_catalog_bytes(ctx: &TaskContext, path: &Path) -> Result<Vec<u8>> {
    if path.extension().and_then(OsStr::to_str) == Some("zst") {
        return Ok(run_capture(ctx.command("zstd").arg("-dc").arg(path))?.into_bytes());
    }
    if path.extension().and_then(OsStr::to_str) == Some("br") {
        return read_brotli_file(path);
    }

    fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

fn read_catalog_v2_from_path(ctx: &TaskContext, path: &Path) -> Result<CatalogV2> {
    let bytes = read_catalog_bytes(ctx, path)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse catalog {}", path.display()))
}

fn read_child_catalog_v2_from_path(ctx: &TaskContext, path: &Path) -> Result<ChildCatalogV2> {
    let bytes = read_catalog_bytes(ctx, path)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse child catalog {}", path.display()))
}

fn read_catalog_v3_from_path(ctx: &TaskContext, path: &Path) -> Result<CatalogV3> {
    let bytes = read_catalog_bytes(ctx, path)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse catalog {}", path.display()))
}

fn read_catalog_v3_snippet_from_path(path: &Path) -> Result<CatalogV3PluginEntry> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("failed to parse catalog-v3 snippet {}", path.display()))
}

fn read_prepared_child_catalog(ctx: &TaskContext, dir: &Path) -> Result<ChildCatalogV2> {
    let paths = catalog_asset_paths(dir);
    if paths.pretty_json.is_file() {
        return read_child_catalog_v2_from_path(ctx, &paths.pretty_json);
    }
    if paths.minified_zst.is_file() {
        return read_child_catalog_v2_from_path(ctx, &paths.minified_zst);
    }
    bail!(
        "prepared child catalog missing {} or {}",
        paths.pretty_json.display(),
        paths.minified_zst.display()
    );
}

fn read_prepared_catalog_v3_snippet(dir: &Path) -> Result<CatalogV3PluginEntry> {
    let path = catalog_v3_snippet_path(dir);
    if !path.is_file() {
        bail!("prepared catalog-v3 snippet missing {}", path.display());
    }
    read_catalog_v3_snippet_from_path(&path)
}

fn read_published_official_catalog(ctx: &TaskContext) -> Result<CatalogV2> {
    let temp = tempfile::tempdir()?;
    let central_path = github_release_download(
        ctx,
        OFFICIAL_GITHUB_REPO,
        CENTRAL_CATALOG_RELEASE_TAG,
        CATALOG_PRETTY_JSON,
        temp.path(),
    )?;
    read_catalog_v2_from_path(ctx, &central_path)
}

fn merge_child_catalog_releases(
    plugin_id: &str,
    releases: Vec<ChildCatalogReleaseV2>,
) -> Result<Vec<ChildCatalogReleaseV2>> {
    let mut by_version = BTreeMap::<Version, ChildCatalogReleaseV2>::new();
    for release in releases {
        let version = Version::parse(&release.version)
            .with_context(|| format!("{plugin_id}: invalid release version {}", release.version))?;
        semver::VersionReq::parse(&release.sdk_constraint).with_context(|| {
            format!(
                "{plugin_id} {}: invalid SDK constraint {}",
                release.version, release.sdk_constraint
            )
        })?;
        if let Some(existing) = by_version.insert(version, release.clone())
            && existing.artifact_manifest_url != release.artifact_manifest_url
        {
            bail!(
                "{plugin_id} {}: child catalog release row points to multiple manifests",
                release.version
            );
        }
    }

    Ok(by_version.into_values().collect())
}

fn write_child_catalog_to_dir(
    ctx: &TaskContext,
    plugin: &LocalPluginInfo,
    extra_release: Option<ChildCatalogReleaseV2>,
    existing_releases: Vec<ChildCatalogReleaseV2>,
    dir: &Path,
) -> Result<CatalogAssetPaths> {
    let mut releases = catalog_v2_supported_child_releases(existing_releases)?;

    if let Some(release) = extra_release {
        releases.retain(|existing| existing.version != release.version);
        releases.push(release);
    }

    let catalog = child_catalog_from_local_plugin(plugin, releases)?;
    write_catalog_assets(ctx, &catalog, dir)
}

fn run_official_release(ctx: &TaskContext, args: OfficialReleaseArgs) -> Result<()> {
    warn(
        "official release now prepares unsigned assets only; CI owns signing and GitHub release publication",
    );
    run_official_prepare(
        ctx,
        OfficialPrepareArgs {
            plugin_id: args.plugin_id,
            version: args.version,
            out: args.out,
            existing_child_catalog: args.existing_child_catalog,
            catalog_version: None,
        },
    )
}

fn resolve_existing_child_catalog_releases(
    ctx: &TaskContext,
    plugin_id: &str,
    existing_child_catalog: Option<&Path>,
) -> Result<Vec<ChildCatalogReleaseV2>> {
    if let Some(path) = existing_child_catalog {
        return read_child_catalog_releases_from_path(ctx, path);
    }

    read_published_child_catalog_releases(ctx, plugin_id)
}

fn prepare_official_release(
    ctx: &TaskContext,
    args: OfficialPrepareArgs,
) -> Result<OfficialPreparedRelease> {
    ensure_current_sdk_dependency_is_published(ctx)?;
    step(format!(
        "Preparing unsigned release assets for {}",
        args.plugin_id
    ));
    let plugin = discover_local_official_plugin(ctx, &args.plugin_id)?;
    let catalog_version = match args.catalog_version {
        Some(version) => {
            if !plugin.catalog_versions.contains(&version) {
                bail!("{} does not publish catalog-{version:?}", plugin.plugin_id);
            }
            version
        }
        None => {
            if plugin.catalog_versions.len() != 1 {
                bail!(
                    "{} publishes multiple catalog lanes; pass --catalog-version v2 or --catalog-version v3",
                    plugin.plugin_id
                );
            }
            *plugin
                .catalog_versions
                .iter()
                .next()
                .expect("catalog_versions should not be empty")
        }
    };
    let lane = match catalog_version {
        CatalogVersion::V2 => PluginArtifactLane::V2,
        CatalogVersion::V3 => PluginArtifactLane::V3,
    };
    if catalog_version == CatalogVersion::V3 && args.existing_child_catalog.is_some() {
        bail!("--existing-child-catalog is only valid for catalog-v2 preparation");
    }
    let existing_releases = if catalog_version == CatalogVersion::V2 {
        resolve_existing_child_catalog_releases(
            ctx,
            &plugin.plugin_id,
            args.existing_child_catalog.as_deref(),
        )?
    } else {
        Vec::new()
    };
    let dist = args
        .out
        .unwrap_or_else(|| default_child_catalog_dir(ctx, &plugin.plugin_id));
    let selected_feature_sets = if catalog_version == CatalogVersion::V2 {
        vec![
            plugin
                .feature_sets
                .iter()
                .find(|feature_set| feature_set.is_baseline())
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "{} publishes catalog-v2 but does not define a baseline wasm variant",
                        plugin.plugin_id
                    )
                })?,
        ]
    } else {
        plugin.feature_sets.clone()
    };
    let primary_feature_set = if catalog_version == CatalogVersion::V2 {
        WasmFeatureSet::baseline()
    } else {
        primary_feature_set(&plugin.feature_sets).clone()
    };
    let variants = selected_feature_sets
        .iter()
        .map(|feature_set| {
            build_prepared_plugin_variant(ctx, &plugin.plugin_dir, &dist, feature_set, lane)
        })
        .collect::<Result<Vec<_>>>()?;
    let descriptor_variant = variants
        .iter()
        .find(|variant| variant.feature_set == primary_feature_set)
        .ok_or_else(|| anyhow!("failed to locate primary prepared plugin variant"))?;
    let descriptor = load_descriptor_from_wasm(&descriptor_variant.optimized_wasm)?;
    validate_descriptor_contract(&descriptor)?;
    let version = args.version.unwrap_or_else(|| descriptor.version.clone());
    let baseline_variant = variants
        .iter()
        .find(|variant| variant.feature_set.is_baseline());
    if catalog_version == CatalogVersion::V2 && baseline_variant.is_none() {
        bail!(
            "{} publishes catalog-v2 but did not produce a baseline wasm variant",
            plugin.plugin_id
        );
    }
    let sdk_constraint = plugin_descriptor_sdk_constraint(&descriptor);
    let manifest = baseline_variant.map(|baseline_variant| PluginManifestV2 {
        schema_version: PLUGIN_MANIFEST_SCHEMA.to_string(),
        id: descriptor.id.clone(),
        plugin_type: descriptor.plugin_type().to_string(),
        provider_type: descriptor.provider_type().to_string(),
        version: version.clone(),
        publisher: "scryer".to_string(),
        artifact: "plugin.wasm.zst".to_string(),
        compression: "zstd".to_string(),
        wasm_digest: blake3_file(&baseline_variant.optimized_wasm)
            .expect("baseline variant wasm digest should be readable"),
        artifact_digest: blake3_file(&baseline_variant.compressed_zst.source_path)
            .expect("baseline variant zstd digest should be readable"),
        signature: "plugin.wasm.zst.bundle".to_string(),
    });
    let manifest_json = if catalog_version == CatalogVersion::V2 {
        let manifest_path = dist.join("plugin.manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(
                manifest
                    .as_ref()
                    .expect("baseline manifest should be present for v2 publishing"),
            )? + "\n",
        )?;
        Some(manifest_path)
    } else {
        None
    };
    let child_catalog = if catalog_version == CatalogVersion::V2 {
        Some(write_child_catalog_to_dir(
            ctx,
            &plugin,
            Some(ChildCatalogReleaseV2 {
                version: manifest
                    .as_ref()
                    .expect("baseline manifest should be present for v2 publishing")
                    .version
                    .clone(),
                sdk_constraint: sdk_constraint.clone(),
                artifact_manifest_url: official_plugin_manifest_url(
                    &plugin.plugin_id,
                    &manifest
                        .as_ref()
                        .expect("baseline manifest should be present for v2 publishing")
                        .version,
                ),
            }),
            existing_releases.clone(),
            &dist,
        )?)
    } else {
        None
    };
    let catalog_v3_snippet = if catalog_version == CatalogVersion::V3 {
        let release = catalog_v3_release_from_prepared_assets(
            &plugin,
            &version,
            &sdk_constraint,
            plugin.min_scryer_version.as_deref(),
            &variants,
        )?;
        Some(write_catalog_v3_snippet(
            &catalog_v3_plugin_entry(&plugin, vec![release]),
            &dist,
        )?)
    } else {
        None
    };
    Ok(OfficialPreparedRelease {
        dist,
        variants,
        manifest_json,
        child_catalog,
        catalog_v3_snippet,
    })
}

fn run_official_prepare(ctx: &TaskContext, args: OfficialPrepareArgs) -> Result<()> {
    let prepared = prepare_official_release(ctx, args)?;
    ok(format!(
        "wrote unsigned release assets to {}",
        prepared.dist.display()
    ));
    for variant in &prepared.variants {
        println!(
            "   Variant  : {} [{}]",
            variant.optimized_wasm.display(),
            if variant.feature_set.is_baseline() {
                "baseline".to_string()
            } else {
                variant
                    .feature_set
                    .required_features
                    .iter()
                    .map(|feature| feature.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            }
        );
        println!(
            "   Artifact : {}",
            variant.compressed_zst.source_path.display()
        );
        println!(
            "   Artifact : {}",
            variant.compressed_br.source_path.display()
        );
        if let Some(staged_path) = &variant.compressed_zst.staged_path {
            println!("   V3 Asset : {}", staged_path.display());
        }
        if let Some(staged_path) = &variant.compressed_br.staged_path {
            println!("   V3 Asset : {}", staged_path.display());
        }
    }
    if let Some(manifest_json) = &prepared.manifest_json {
        println!("   Manifest : {}", manifest_json.display());
    }
    if let Some(child_catalog) = &prepared.child_catalog {
        println!("   Catalog  : {}", child_catalog.pretty_json.display());
        println!("   Runtime  : {}", child_catalog.minified_zst.display());
    }
    if let Some(catalog_v3_snippet) = &prepared.catalog_v3_snippet {
        println!("   V3 Snip. : {}", catalog_v3_snippet.display());
    }
    Ok(())
}

fn run_official_prefetch(ctx: &TaskContext, args: OfficialPrefetchArgs) -> Result<()> {
    if args.plugin_ids.is_empty() {
        bail!("official prefetch requires at least one plugin id");
    }

    let plugins = official_plugin_dirs_by_id(ctx)?;
    let mut selected = BTreeSet::new();
    for plugin_id in args.plugin_ids {
        if !selected.insert(plugin_id.clone()) {
            continue;
        }

        let plugin_dir = plugins
            .get(&plugin_id)
            .ok_or_else(|| anyhow!("plugin '{plugin_id}' not found in local official plugins"))?;
        prefetch_plugin_dependencies(ctx, plugin_dir)?;
    }

    ok("prefetched plugin release dependencies");
    Ok(())
}

fn run_official_plan_current(ctx: &TaskContext, args: OfficialPlanCurrentArgs) -> Result<()> {
    if args.plugin_ids.is_empty() {
        bail!("official plan-current requires at least one plugin id");
    }

    let plugin_dirs = official_plugin_dirs_by_id(ctx)?;
    let mut selected = BTreeSet::new();
    for plugin_id in args.plugin_ids {
        if !selected.insert(plugin_id.clone()) {
            continue;
        }

        let plugin_dir = plugin_dirs
            .get(&plugin_id)
            .ok_or_else(|| anyhow!("plugin '{plugin_id}' not found in local official plugins"))?;
        let cargo_toml = plugin_dir.join("Cargo.toml");
        let metadata = plugin_manifest_metadata(&cargo_toml)?;
        if !metadata.catalog_versions.contains(&CatalogVersion::V3) {
            bail!("{plugin_id} does not publish catalog-v3");
        }
        println!("{}\t{}", plugin_id, package_version(&cargo_toml)?);
    }

    Ok(())
}

fn run_official_plan_changed(ctx: &TaskContext, args: OfficialPlanChangedArgs) -> Result<()> {
    let plugin_ids = official_plugin_dirs_by_id(ctx)?
        .into_keys()
        .collect::<Vec<_>>();
    run_official_prefetch(ctx, OfficialPrefetchArgs { plugin_ids })?;

    let options = release_options_from_plan_args(&args);
    let plans = collect_changed_release_targets(ctx, &options)?;
    if plans.is_empty() {
        ok("No official plugin changes detected since per-plugin release tags");
        return Ok(());
    }

    step("Planned changed official plugin releases");
    let mut output = String::new();
    for plan in &plans {
        println!(
            "   {} {} ({})",
            plan.target.plugin_id, plan.target.next_version, plan.reason
        );
        output.push_str(&format!(
            "{}\t{}\n",
            plan.target.plugin_id, plan.target.next_version
        ));
    }

    if let Some(path) = args.out {
        fs::write(&path, output)?;
        ok(format!("Wrote release plan to {}", path.display()));
    } else {
        print!("{output}");
    }

    Ok(())
}

fn run_official_verify_prepared(ctx: &TaskContext, dir: &Path) -> Result<()> {
    step(format!(
        "Verifying prepared release assets in {}",
        dir.display()
    ));
    let manifest_path = dir.join("plugin.manifest.json");
    let catalog_paths = catalog_asset_paths(dir);
    let catalog_v3_snippet = catalog_v3_snippet_path(dir);

    let has_v2_assets = manifest_path.is_file()
        || catalog_paths.pretty_json.is_file()
        || catalog_paths.minified_json.is_file()
        || catalog_paths.minified_zst.is_file();
    let has_v3_assets = catalog_v3_snippet.is_file();

    if !has_v2_assets && !has_v3_assets {
        bail!("prepared release is missing both catalog-v2 and catalog-v3 assets");
    }
    if has_v2_assets
        && !(manifest_path.is_file()
            && catalog_paths.pretty_json.is_file()
            && catalog_paths.minified_json.is_file()
            && catalog_paths.minified_zst.is_file())
    {
        bail!("prepared release has an incomplete catalog-v2 asset set");
    }

    if has_v2_assets {
        let compressed_wasm = dir.join("plugin.wasm.zst");
        let wasm = dir.join("plugin.wasm");
        for path in [&compressed_wasm, &wasm] {
            if !path.is_file() {
                bail!("prepared asset is missing: {}", path.display());
            }
        }
        let manifest: PluginManifestV2 = serde_json::from_slice(&fs::read(&manifest_path)?)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        require_blake3_file(
            "compressed artifact",
            &manifest.artifact_digest,
            &compressed_wasm,
        )?;
        require_blake3_file("decompressed WASM", &manifest.wasm_digest, &wasm)?;

        let pretty_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_paths.pretty_json)?).with_context(|| {
                format!("failed to parse {}", catalog_paths.pretty_json.display())
            })?;
        let runtime_value: serde_json::Value =
            serde_json::from_slice(&read_catalog_bytes(ctx, &catalog_paths.minified_zst)?)?;
        if pretty_value != runtime_value {
            bail!("pretty child catalog and minified zstd child catalog decode to different JSON");
        }
    }

    if has_v3_assets {
        for legacy_asset in [
            "plugin.wasm.zst",
            "plugin.wasm.br",
            "plugin.manifest.json",
            CATALOG_PRETTY_JSON,
            CATALOG_MINIFIED_JSON,
            CATALOG_MINIFIED_ZST,
        ] {
            let path = dir.join(legacy_asset);
            if path.is_file() {
                bail!(
                    "catalog-v3 prepared release must not contain legacy catalog-v2 asset {}",
                    path.display()
                );
            }
        }
        let snippet = read_catalog_v3_snippet_from_path(&catalog_v3_snippet)?;
        validate_catalog_v3_plugin_entry(&snippet)?;
        let expected_id;
        let expected_version;
        let expected_sdk_constraint;
        if has_v2_assets {
            let wasm = dir.join("plugin.wasm");
            let manifest: PluginManifestV2 = serde_json::from_slice(&fs::read(&manifest_path)?)
                .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
            expected_id = manifest.id;
            expected_version = manifest.version;
            let descriptor = load_descriptor_from_wasm(&wasm)?;
            expected_sdk_constraint = plugin_descriptor_sdk_constraint(&descriptor);
        } else {
            let bootstrap_release = snippet
                .releases
                .iter()
                .find(|release| {
                    !snippet_artifacts_are_missing(&release.artifacts, dir).unwrap_or(true)
                })
                .ok_or_else(|| {
                    anyhow!("catalog-v3 snippet does not reference any prepared artifacts")
                })?;
            let bootstrap_artifact = bootstrap_release
                .artifacts
                .first()
                .ok_or_else(|| anyhow!("catalog-v3 snippet must include at least one artifact"))?;
            let bootstrap_artifact_name = url_file_name(&bootstrap_artifact.url)?;
            let bootstrap_artifact_path = dir.join(&bootstrap_artifact_name);
            let bootstrap_dir = tempfile::tempdir()?;
            let bootstrap_wasm = bootstrap_dir.path().join("bootstrap.wasm");
            decompress_plugin_wasm_artifact(ctx, &bootstrap_artifact_path, &bootstrap_wasm)?;
            let descriptor = load_descriptor_from_wasm(&bootstrap_wasm)?;
            validate_descriptor_contract(&descriptor)?;
            expected_id = descriptor.id.clone();
            expected_version = descriptor.version.clone();
            expected_sdk_constraint = plugin_descriptor_sdk_constraint(&descriptor);
        }

        if snippet.id != expected_id {
            bail!("catalog-v3 snippet id does not match prepared plugin id");
        }
        let Some(snippet_release) = snippet
            .releases
            .iter()
            .find(|release| release.version == expected_version)
        else {
            bail!("catalog-v3 snippet is missing release {}", expected_version);
        };
        if snippet_release.sdk_constraint != expected_sdk_constraint {
            bail!("catalog-v3 snippet sdk_constraint does not match prepared plugin");
        }
        if snippet_artifacts_are_missing(&snippet_release.artifacts, dir)? {
            bail!("catalog-v3 snippet references missing prepared artifacts");
        }
        let artifact_temp = tempfile::tempdir()?;
        for (index, snippet_artifact) in snippet_release.artifacts.iter().enumerate() {
            let artifact_name = url_file_name(&snippet_artifact.url)?;
            let artifact_path = dir.join(&artifact_name);
            let wasm_path = artifact_temp.path().join(format!("plugin-{index}.wasm"));
            decompress_plugin_wasm_artifact(ctx, &artifact_path, &wasm_path)?;
            validate_catalog_v3_release_artifact(snippet_artifact, &artifact_path, &wasm_path)?;
            if !snippet_artifact
                .signature_url
                .ends_with(&format!("{artifact_name}.bundle.zst"))
            {
                bail!(
                    "catalog-v3 snippet signature_url must end with {}.bundle.zst",
                    artifact_name
                );
            }
        }
    }

    ok("prepared assets are internally consistent");
    Ok(())
}

fn catalog_entry_from_local_plugin(plugin: &LocalPluginInfo) -> Result<CatalogV2Entry> {
    let version = plugin_crate_version(&plugin.plugin_dir)?;
    Ok(CatalogV2Entry {
        id: plugin.plugin_id.clone(),
        name: plugin.name.clone(),
        description: plugin.description.clone(),
        plugin_type: plugin.plugin_type.clone(),
        provider_type: plugin.provider_type.clone(),
        publisher: "scryer".to_string(),
        support_tier: "official".to_string(),
        docs_url: plugin.docs_url.clone(),
        source_repo: plugin.source_repo.clone(),
        child_catalog_url: official_plugin_child_catalog_url(&plugin.plugin_id, &version),
        required_signer: RequiredSignerV2 {
            github_repository: OFFICIAL_GITHUB_REPO.to_string(),
            github_workflow: Some(official_release_workflow()),
        },
    })
}

fn catalog_entry_from_child_catalog(catalog: &ChildCatalogV2) -> Result<CatalogV2Entry> {
    let release = latest_child_catalog_release(catalog)?;
    Ok(CatalogV2Entry {
        id: catalog.id.clone(),
        name: catalog.name.clone(),
        description: catalog.description.clone(),
        plugin_type: catalog.plugin_type.clone(),
        provider_type: catalog.provider_type.clone(),
        publisher: catalog.publisher.clone(),
        support_tier: catalog.support_tier.clone(),
        docs_url: catalog.docs_url.clone(),
        source_repo: catalog.source_repo.clone(),
        child_catalog_url: official_plugin_child_catalog_url(&catalog.id, &release.version),
        required_signer: RequiredSignerV2 {
            github_repository: OFFICIAL_GITHUB_REPO.to_string(),
            github_workflow: Some(official_release_workflow()),
        },
    })
}

fn merge_catalog_plugin_entries(
    existing: Vec<CatalogV2Entry>,
    updates: Vec<CatalogV2Entry>,
) -> Vec<CatalogV2Entry> {
    let mut by_id = BTreeMap::new();
    for entry in existing {
        by_id.insert(entry.id.clone(), entry);
    }
    for entry in updates {
        by_id.insert(entry.id.clone(), entry);
    }
    by_id.into_values().collect()
}

fn official_required_signer() -> RequiredSignerV2 {
    RequiredSignerV2 {
        github_repository: OFFICIAL_GITHUB_REPO.to_string(),
        github_workflow: Some(official_release_workflow()),
    }
}

fn rule_pack_artifact_file_name(rule_pack_id: &str, compression_suffix: &str) -> String {
    format!("{rule_pack_id}.min.json.{compression_suffix}")
}

fn rule_pack_minified_json_file_name(rule_pack_id: &str) -> String {
    format!("{rule_pack_id}.min.json")
}

fn stage_hashed_asset_copy(
    source_path: &Path,
    logical_name: &str,
    output_dir: &Path,
) -> Result<(PathBuf, Vec<String>)> {
    let digests = file_digests(source_path)?;
    let file_name = hashed_filename(logical_name, &digests)?;
    let output_path = output_dir.join(file_name);
    fs::copy(source_path, &output_path).with_context(|| {
        format!(
            "failed to stage hashed asset copy {}",
            output_path.display()
        )
    })?;
    Ok((output_path, digests))
}

fn catalog_v3_artifact_from_staged_file(
    digests: Vec<String>,
    primary_url: String,
    mirror_urls: Vec<String>,
) -> Result<CatalogV3Artifact> {
    let signature_url = format!("{primary_url}.bundle.zst");
    let signature_mirror_urls = mirror_urls
        .iter()
        .map(|url| format!("{url}.bundle.zst"))
        .collect::<Vec<_>>();
    Ok(CatalogV3Artifact {
        url: primary_url,
        mirror_urls,
        signature_url,
        signature_mirror_urls,
        digests,
    })
}

fn catalog_v3_plugin_artifact_from_staged_file(
    feature_set: &WasmFeatureSet,
    wasm_digests: Vec<String>,
    bytes: u64,
    digests: Vec<String>,
    primary_url: String,
    mirror_urls: Vec<String>,
) -> Result<CatalogV3PluginArtifact> {
    let location = catalog_v3_artifact_from_staged_file(digests, primary_url, mirror_urls)?;
    Ok(CatalogV3PluginArtifact {
        runtime: WASM_TARGET.to_string(),
        required_features: feature_set.required_features.clone(),
        wasm_digests,
        bytes,
        url: location.url,
        mirror_urls: location.mirror_urls,
        signature_url: location.signature_url,
        signature_mirror_urls: location.signature_mirror_urls,
        digests: location.digests,
    })
}

fn build_prepared_plugin_variant(
    ctx: &TaskContext,
    plugin_dir: &Path,
    dist: &Path,
    feature_set: &WasmFeatureSet,
    lane: PluginArtifactLane,
) -> Result<PreparedPluginVariant> {
    let wasm = build_plugin_wasm(ctx, plugin_dir, feature_set)?;
    let (optimized, compressed_zst_source, compressed_br_source) =
        optimize_and_compress_wasm(ctx, &wasm, dist, feature_set, lane)?;
    let (staged_zst, zst_digests) = if lane == PluginArtifactLane::V3 {
        let (path, digests) = stage_hashed_asset_copy(
            &compressed_zst_source,
            &plugin_variant_logical_file_name(feature_set, lane, "zst"),
            dist,
        )?;
        (Some(path), digests)
    } else {
        (None, file_digests(&compressed_zst_source)?)
    };
    let (staged_br, br_digests) = if lane == PluginArtifactLane::V3 {
        let (path, digests) = stage_hashed_asset_copy(
            &compressed_br_source,
            &plugin_variant_logical_file_name(feature_set, lane, "br"),
            dist,
        )?;
        (Some(path), digests)
    } else {
        (None, file_digests(&compressed_br_source)?)
    };

    Ok(PreparedPluginVariant {
        feature_set: feature_set.clone(),
        bytes: fs::metadata(&optimized)
            .with_context(|| format!("failed to stat {}", optimized.display()))?
            .len(),
        wasm_digests: file_digests(&optimized)?,
        optimized_wasm: optimized,
        compressed_zst: PreparedCompressedArtifact {
            source_path: compressed_zst_source,
            staged_path: staged_zst,
            digests: zst_digests,
        },
        compressed_br: PreparedCompressedArtifact {
            source_path: compressed_br_source,
            staged_path: staged_br,
            digests: br_digests,
        },
    })
}

fn merge_catalog_v3_plugin_entries(
    existing: Vec<CatalogV3PluginEntry>,
    updates: Vec<CatalogV3PluginEntry>,
) -> Vec<CatalogV3PluginEntry> {
    let mut by_id = BTreeMap::new();
    for entry in existing {
        by_id.insert(entry.id.clone(), entry);
    }
    for entry in updates {
        by_id.insert(entry.id.clone(), entry);
    }
    by_id.into_values().collect()
}

fn catalog_v3_release_from_prepared_assets(
    plugin: &LocalPluginInfo,
    version: &str,
    sdk_constraint: &str,
    min_scryer_version: Option<&str>,
    variants: &[PreparedPluginVariant],
) -> Result<CatalogV3Release> {
    let mut artifacts = Vec::new();
    let distribution_base_url = official_plugin_v3_distribution_base_url(plugin);
    for variant in variants {
        for compressed_artifact in [&variant.compressed_zst, &variant.compressed_br] {
            let staged_path = compressed_artifact.staged_path.as_ref().ok_or_else(|| {
                anyhow!(
                    "{} catalog-v3 release is missing staged v3 artifact for {}",
                    plugin.plugin_id,
                    compressed_artifact.source_path.display()
                )
            })?;
            let file_name = compressed_artifact
                .staged_path
                .as_ref()
                .expect("staged_path should be present")
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow!("invalid artifact path {}", staged_path.display()))?
                .to_string();
            let primary_url =
                versioned_distribution_url(&distribution_base_url, version, &file_name);
            artifacts.push(catalog_v3_plugin_artifact_from_staged_file(
                &variant.feature_set,
                variant.wasm_digests.clone(),
                variant.bytes,
                compressed_artifact.digests.clone(),
                primary_url,
                official_plugin_v3_github_mirror_urls(&plugin.plugin_id, version, &file_name),
            )?);
        }
    }
    Ok(CatalogV3Release {
        version: version.to_string(),
        sdk_constraint: sdk_constraint.to_string(),
        min_scryer_version: min_scryer_version.map(str::to_string),
        artifacts,
    })
}

fn catalog_v3_plugin_entry(
    plugin: &LocalPluginInfo,
    releases: Vec<CatalogV3Release>,
) -> CatalogV3PluginEntry {
    CatalogV3PluginEntry {
        id: plugin.plugin_id.clone(),
        name: plugin.name.clone(),
        description: plugin.description.clone(),
        plugin_type: plugin.plugin_type.clone(),
        provider_type: plugin.provider_type.clone(),
        publisher: "scryer".to_string(),
        support_tier: "official".to_string(),
        status: plugin.status,
        docs_url: plugin.docs_url.clone(),
        source_repo: plugin.source_repo.clone(),
        required_signer: official_required_signer(),
        releases,
    }
}

fn catalog_v3_entry_from_current_plugin_build(
    ctx: &TaskContext,
    plugin: &LocalPluginInfo,
    dist: &Path,
) -> Result<CatalogV3PluginEntry> {
    let primary_feature_set = primary_feature_set(&plugin.feature_sets).clone();
    let variants = plugin
        .feature_sets
        .iter()
        .map(|feature_set| {
            build_prepared_plugin_variant(
                ctx,
                &plugin.plugin_dir,
                dist,
                feature_set,
                PluginArtifactLane::V3,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let descriptor_variant = variants
        .iter()
        .find(|variant| variant.feature_set == primary_feature_set)
        .ok_or_else(|| anyhow!("failed to locate primary prepared plugin variant"))?;
    let descriptor = load_descriptor_from_wasm(&descriptor_variant.optimized_wasm)?;
    validate_descriptor_contract(&descriptor)?;
    let release = catalog_v3_release_from_prepared_assets(
        plugin,
        &descriptor.version,
        &plugin_descriptor_sdk_constraint(&descriptor),
        plugin.min_scryer_version.as_deref(),
        &variants,
    )?;
    Ok(catalog_v3_plugin_entry(plugin, vec![release]))
}

fn rule_pack_asset_url(asset_name: &str) -> String {
    github_release_asset_url(
        OFFICIAL_GITHUB_REPO,
        CENTRAL_CATALOG_RELEASE_TAG,
        asset_name,
    )
}

fn load_rule_pack_manifest(path: &Path) -> Result<RulePackManifestV1> {
    let manifest: RulePackManifestV1 = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if manifest.schema_version != 1 {
        bail!(
            "{}: unsupported rule pack schema {}",
            path.display(),
            manifest.schema_version
        );
    }
    if manifest.id.trim().is_empty() {
        bail!("{}: rule pack id is required", path.display());
    }
    if manifest.name.trim().is_empty() {
        bail!("{}: rule pack name is required", path.display());
    }
    if manifest.author.trim().is_empty() {
        bail!("{}: rule pack author is required", path.display());
    }
    Version::parse(manifest.version.trim()).with_context(|| {
        format!(
            "{}: invalid rule pack version {}",
            path.display(),
            manifest.version
        )
    })?;
    if manifest.rules.is_empty() {
        bail!(
            "{}: rule pack must contain at least one rule",
            path.display()
        );
    }
    Ok(manifest)
}

fn load_rule_pack_catalog_entries(ctx: &TaskContext) -> Result<Vec<PreparedRulePack>> {
    let manifest_path = ctx.repo_root.join(RULE_PACK_SOURCE_MANIFEST);
    let source: RulePackSourceManifest = serde_json::from_slice(&fs::read(&manifest_path)?)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let mut prepared = Vec::new();
    let mut ids = BTreeSet::new();
    for rule_pack in source.rule_packs {
        let asset_name = Path::new(&rule_pack.asset)
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("invalid rule pack asset {}", rule_pack.asset))?
            .to_string();
        if asset_name != rule_pack.asset {
            bail!(
                "rule pack asset '{}' must be a bare filename inside rule_packs/",
                rule_pack.asset
            );
        }
        if let Some(min_scryer_version) = rule_pack.min_scryer_version.as_deref() {
            Version::parse(min_scryer_version.trim()).with_context(|| {
                format!(
                    "rule pack asset '{}' has invalid min_scryer_version {}",
                    asset_name, min_scryer_version
                )
            })?;
        }
        if rule_pack.id.trim().is_empty() {
            bail!("rule pack source entry for '{}' is missing id", asset_name);
        }
        if rule_pack.distribution_base_url.trim().is_empty() {
            bail!(
                "rule pack source entry for '{}' is missing distribution_base_url",
                asset_name
            );
        }
        let source_path = ctx.repo_root.join("rule_packs").join(&asset_name);
        let manifest = load_rule_pack_manifest(&source_path)?;
        if manifest.id != rule_pack.id {
            bail!(
                "rule pack source id '{}' does not match manifest id '{}'",
                rule_pack.id,
                manifest.id
            );
        }
        if !ids.insert(manifest.id.clone()) {
            bail!("duplicate rule pack id {}", manifest.id);
        }
        prepared.push(PreparedRulePack {
            entry: RulePackCatalogEntryV2 {
                id: manifest.id,
                name: manifest.name,
                description: manifest.description,
                author: manifest.author,
                version: manifest.version,
                url: rule_pack_asset_url(&asset_name),
                min_scryer_version: rule_pack.min_scryer_version,
            },
            source_path,
            asset_name,
        });
    }
    Ok(prepared)
}

fn prepare_rule_pack_v3_entries(
    ctx: &TaskContext,
    output_dir: &Path,
) -> Result<Vec<PreparedRulePackV3>> {
    fs::create_dir_all(output_dir)?;
    let manifest_path = ctx.repo_root.join(RULE_PACK_SOURCE_MANIFEST);
    let source: RulePackSourceManifest = serde_json::from_slice(&fs::read(&manifest_path)?)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let mut prepared = Vec::new();
    let mut ids = BTreeSet::new();

    for rule_pack in source.rule_packs {
        let asset_name = Path::new(&rule_pack.asset)
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("invalid rule pack asset {}", rule_pack.asset))?
            .to_string();
        let source_path = ctx.repo_root.join("rule_packs").join(&asset_name);
        let manifest = load_rule_pack_manifest(&source_path)?;
        if manifest.id != rule_pack.id {
            bail!(
                "rule pack source id '{}' does not match manifest id '{}'",
                rule_pack.id,
                manifest.id
            );
        }
        if !ids.insert(manifest.id.clone()) {
            bail!("duplicate rule pack id {}", manifest.id);
        }
        let base_name = rule_pack_minified_json_file_name(&manifest.id);
        let minified_json_path = output_dir.join(&base_name);
        let minified = serde_json::to_string(
            &serde_json::from_slice::<serde_json::Value>(&fs::read(&source_path)?)
                .with_context(|| format!("failed to parse {}", source_path.display()))?,
        )?;
        fs::write(&minified_json_path, minified)
            .with_context(|| format!("failed to write {}", minified_json_path.display()))?;

        let zst_source = output_dir.join(rule_pack_artifact_file_name(&manifest.id, "zst"));
        run_checked(
            ctx.command("zstd")
                .arg(ZSTD_LEVEL)
                .arg("-f")
                .arg(&minified_json_path)
                .arg("-o")
                .arg(&zst_source),
        )?;
        let br_source = output_dir.join(rule_pack_artifact_file_name(&manifest.id, "br"));
        write_brotli_file(&minified_json_path, &br_source)?;
        let (hashed_zst, zst_digests) = stage_hashed_asset_copy(
            &zst_source,
            &rule_pack_artifact_file_name(&manifest.id, "zst"),
            output_dir,
        )?;
        let (hashed_br, br_digests) = stage_hashed_asset_copy(
            &br_source,
            &rule_pack_artifact_file_name(&manifest.id, "br"),
            output_dir,
        )?;
        let version = manifest.version.clone();
        let primary_base = trim_url_base(&rule_pack.distribution_base_url);
        let zst_name = hashed_zst
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("invalid staged rule pack asset {}", hashed_zst.display()))?
            .to_string();
        let br_name = hashed_br
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("invalid staged rule pack asset {}", hashed_br.display()))?
            .to_string();
        prepared.push(PreparedRulePackV3 {
            entry: CatalogV3RulePackEntry {
                id: manifest.id.clone(),
                name: manifest.name,
                description: manifest.description,
                author: manifest.author,
                releases: vec![CatalogV3RulePackRelease {
                    version: version.clone(),
                    min_scryer_version: rule_pack.min_scryer_version,
                    rule_pack_digests: file_digests(&minified_json_path)?,
                    artifacts: vec![
                        CatalogV3Artifact {
                            url: versioned_distribution_url(&primary_base, &version, &zst_name),
                            mirror_urls: vec![github_release_asset_url(
                                OFFICIAL_GITHUB_REPO,
                                &central_catalog_v3_release_tag(),
                                &zst_name,
                            )],
                            signature_url: versioned_distribution_url(
                                &primary_base,
                                &version,
                                &signature_bundle_file_name(&zst_name),
                            ),
                            signature_mirror_urls: vec![github_release_asset_url(
                                OFFICIAL_GITHUB_REPO,
                                &central_catalog_v3_release_tag(),
                                &signature_bundle_file_name(&zst_name),
                            )],
                            digests: zst_digests,
                        },
                        CatalogV3Artifact {
                            url: versioned_distribution_url(&primary_base, &version, &br_name),
                            mirror_urls: vec![github_release_asset_url(
                                OFFICIAL_GITHUB_REPO,
                                &central_catalog_v3_release_tag(),
                                &br_name,
                            )],
                            signature_url: versioned_distribution_url(
                                &primary_base,
                                &version,
                                &signature_bundle_file_name(&br_name),
                            ),
                            signature_mirror_urls: vec![github_release_asset_url(
                                OFFICIAL_GITHUB_REPO,
                                &central_catalog_v3_release_tag(),
                                &signature_bundle_file_name(&br_name),
                            )],
                            digests: br_digests,
                        },
                    ],
                }],
            },
        });
    }

    Ok(prepared)
}

fn stage_rule_pack_assets(rule_packs: &[PreparedRulePack], output_dir: &Path) -> Result<()> {
    for rule_pack in rule_packs {
        fs::copy(
            &rule_pack.source_path,
            output_dir.join(&rule_pack.asset_name),
        )
        .with_context(|| {
            format!(
                "failed to stage rule pack asset {}",
                rule_pack.source_path.display()
            )
        })?;
    }
    Ok(())
}

fn run_catalog_render_v2(ctx: &TaskContext) -> Result<()> {
    run_catalog_prepare_v2(
        ctx,
        CatalogPrepareV2Args {
            out: None,
            plugin_ids: Vec::new(),
            existing_catalog: None,
            prepared_child_catalog_root: None,
        },
    )
}

fn run_catalog_render_v3(ctx: &TaskContext) -> Result<()> {
    run_catalog_prepare_v3(
        ctx,
        CatalogPrepareV3Args {
            out: None,
            plugin_ids: Vec::new(),
            existing_catalog: None,
            prepared_plugin_root: None,
            allow_selected_rebuild: false,
        },
    )
}

fn run_catalog_prepare_v2(ctx: &TaskContext, args: CatalogPrepareV2Args) -> Result<()> {
    let plugins = if args.plugin_ids.is_empty() {
        step("Preparing catalog-v2 assets from local official plugin descriptors");
        discover_local_plugins(ctx)?
            .iter()
            .filter(|plugin| plugin_publishes_catalog(plugin, CatalogVersion::V2))
            .map(catalog_entry_from_local_plugin)
            .collect::<Result<Vec<_>>>()?
    } else {
        if args.prepared_child_catalog_root.is_some() {
            step("Preparing catalog-v2 assets from prepared official child catalogs");
        } else {
            step("Preparing catalog-v2 assets for selected official plugin descriptors");
        }
        let mut selected = BTreeSet::new();
        let mut updates = Vec::new();
        for plugin_id in &args.plugin_ids {
            if !selected.insert(plugin_id.clone()) {
                continue;
            }
            let plugin = discover_local_official_plugin(ctx, plugin_id)?;
            if !plugin_publishes_catalog(&plugin, CatalogVersion::V2) {
                continue;
            }
            if let Some(root) = args.prepared_child_catalog_root.as_deref() {
                let child_catalog = read_prepared_child_catalog(ctx, &root.join(plugin_id))?;
                if child_catalog.id != *plugin_id {
                    bail!(
                        "prepared child catalog at {} has id '{}' but expected '{}'",
                        root.join(plugin_id).display(),
                        child_catalog.id,
                        plugin_id
                    );
                }
                updates.push(catalog_entry_from_child_catalog(&child_catalog)?);
            } else {
                updates.push(catalog_entry_from_local_plugin(&plugin)?);
            }
        }
        let base_catalog = match args.existing_catalog.as_deref() {
            Some(path) => read_catalog_v2_from_path(ctx, path)?,
            None => read_published_official_catalog(ctx)?,
        };
        merge_catalog_plugin_entries(base_catalog.plugins, updates)
    };
    let rule_packs = load_rule_pack_catalog_entries(ctx)?;
    let catalog = CatalogV2 {
        schema_version: CATALOG_V2_SCHEMA.to_string(),
        plugins,
        rule_packs: rule_packs
            .iter()
            .map(|rule_pack| rule_pack.entry.clone())
            .collect(),
    };
    validate_official_catalog(&catalog)?;
    let dist = args
        .out
        .unwrap_or_else(|| ctx.repo_root.join("dist").join("catalog-v2"));
    let central_paths = write_catalog_assets(ctx, &catalog, &dist)?;
    stage_rule_pack_assets(&rule_packs, &dist)?;
    ok(format!("wrote {}", central_paths.pretty_json.display()));
    ok(format!("wrote {}", central_paths.minified_zst.display()));
    Ok(())
}

fn run_catalog_prepare_v3(ctx: &TaskContext, args: CatalogPrepareV3Args) -> Result<()> {
    let dist = args
        .out
        .clone()
        .unwrap_or_else(|| default_central_catalog_v3_dir(ctx));
    let prepared_rule_packs = prepare_rule_pack_v3_entries(ctx, &dist)?;
    let existing_catalog = match args.existing_catalog.as_deref() {
        Some(path) => Some(read_catalog_v3_from_path(ctx, path)?),
        None => None,
    };
    if !args.plugin_ids.is_empty() && existing_catalog.is_none() && !args.allow_selected_rebuild {
        bail!(
            "catalog prepare-v3 with --plugin-id requires --existing-catalog; use --allow-selected-rebuild only for an intentional full catalog rebuild from prepared plugin snippets"
        );
    }
    if args.allow_selected_rebuild && args.prepared_plugin_root.is_none() {
        bail!("catalog prepare-v3 --allow-selected-rebuild requires --prepared-plugin-root");
    }
    if args.allow_selected_rebuild && args.plugin_ids.is_empty() {
        bail!("catalog prepare-v3 --allow-selected-rebuild requires at least one --plugin-id");
    }
    if args.prepared_plugin_root.is_none() {
        ensure_current_sdk_dependency_is_published(ctx)?;
    }
    let catalog_version = existing_catalog
        .as_ref()
        .map(|catalog| catalog.catalog_version.max(1) + 1)
        .unwrap_or(1);
    let plugins = if args.plugin_ids.is_empty() {
        step("Preparing catalog-v3 assets from current official plugin builds");
        let mut entries = Vec::new();
        for plugin in discover_local_plugins(ctx)? {
            if !plugin_publishes_catalog(&plugin, CatalogVersion::V3) {
                continue;
            }
            if let Some(root) = args.prepared_plugin_root.as_deref() {
                let prepared_dir = root.join(&plugin.plugin_id);
                if prepared_dir.is_dir() {
                    entries.push(read_prepared_catalog_v3_snippet(&prepared_dir)?);
                    continue;
                }
            }
            entries.push(catalog_v3_entry_from_current_plugin_build(
                ctx,
                &plugin,
                &dist.join("plugins").join(&plugin.plugin_id),
            )?);
        }
        entries
    } else {
        if args.prepared_plugin_root.is_some() {
            step("Preparing catalog-v3 assets from prepared plugin snippets");
        } else {
            step("Preparing catalog-v3 assets from selected current official plugin builds");
        }
        let mut selected = BTreeSet::new();
        let mut updates = Vec::new();
        for plugin_id in &args.plugin_ids {
            if !selected.insert(plugin_id.clone()) {
                continue;
            }
            if let Some(root) = args.prepared_plugin_root.as_deref() {
                if !official_plugin_catalog_versions(ctx, plugin_id)?.contains(&CatalogVersion::V3)
                {
                    continue;
                }
                updates.push(read_prepared_catalog_v3_snippet(&root.join(plugin_id))?);
            } else {
                let plugin = discover_local_official_plugin(ctx, plugin_id)?;
                if !plugin_publishes_catalog(&plugin, CatalogVersion::V3) {
                    continue;
                }
                updates.push(catalog_v3_entry_from_current_plugin_build(
                    ctx,
                    &plugin,
                    &dist.join("plugins").join(&plugin.plugin_id),
                )?);
            }
        }
        let base_plugins = existing_catalog
            .clone()
            .map(|catalog| catalog.plugins)
            .unwrap_or_default();
        merge_catalog_v3_plugin_entries(base_plugins, updates)
    };

    let catalog = CatalogV3 {
        schema_version: CATALOG_V3_SCHEMA.to_string(),
        catalog_version,
        plugins,
        rule_packs: prepared_rule_packs
            .iter()
            .map(|rule_pack| rule_pack.entry.clone())
            .collect(),
    };
    validate_catalog_v3(&catalog)?;
    let central_paths = write_catalog_v3_assets(ctx, &catalog, &dist)?;
    let staged_central_artifacts =
        stage_hashed_central_catalog_v3_artifacts(&central_paths, &dist, catalog_version)?;
    let redirect_path = write_catalog_v3_redirects(
        &CatalogV3Redirect {
            schema_version: CATALOG_V3_REDIRECT_SCHEMA.to_string(),
            catalog_version,
            artifacts: staged_central_artifacts
                .iter()
                .map(|(_, artifact)| CatalogV3RedirectArtifact {
                    url: artifact.url.clone(),
                    mirror_urls: artifact.mirror_urls.clone(),
                    signature_url: artifact.signature_url.clone(),
                    signature_mirror_urls: artifact.signature_mirror_urls.clone(),
                })
                .collect(),
        },
        &central_paths,
    )?;
    ok(format!("wrote {}", central_paths.minified_zst.display()));
    ok(format!("wrote {}", redirect_path.display()));
    Ok(())
}

fn run_catalog_publish_v2(ctx: &TaskContext) -> Result<()> {
    warn(
        "catalog publish-v2 now prepares unsigned assets only; CI owns signing and GitHub release publication",
    );
    run_catalog_prepare_v2(
        ctx,
        CatalogPrepareV2Args {
            out: None,
            plugin_ids: Vec::new(),
            existing_catalog: None,
            prepared_child_catalog_root: None,
        },
    )
}

fn run_catalog_publish_v3(ctx: &TaskContext) -> Result<()> {
    warn(
        "catalog publish-v3 now prepares unsigned assets only; CI owns signing and GitHub release publication",
    );
    run_catalog_prepare_v3(
        ctx,
        CatalogPrepareV3Args {
            out: None,
            plugin_ids: Vec::new(),
            existing_catalog: None,
            prepared_plugin_root: None,
            allow_selected_rebuild: false,
        },
    )
}

struct R2Config {
    endpoint_url: String,
    bucket: String,
    access_key_id: String,
    secret_access_key: String,
}

fn r2_config_from_env() -> Result<R2Config> {
    let account_id = first_nonempty_env(&[R2_ACCOUNT_ID_ENV, R2_ACCOUNT_ID_ENV_LEGACY])
        .ok_or_else(|| {
            anyhow!("{R2_ACCOUNT_ID_ENV} or {R2_ACCOUNT_ID_ENV_LEGACY} must be set for R2 uploads")
        })?;
    let bucket = first_nonempty_env(&[R2_BUCKET_ENV, R2_BUCKET_ENV_LEGACY]).ok_or_else(|| {
        anyhow!("{R2_BUCKET_ENV} or {R2_BUCKET_ENV_LEGACY} must be set for R2 uploads")
    })?;
    let access_key_id = first_nonempty_env(&[R2_ACCESS_KEY_ID_ENV])
        .ok_or_else(|| anyhow!("{R2_ACCESS_KEY_ID_ENV} must be set for R2 uploads"))?;
    let secret_access_key = first_nonempty_env(&[R2_SECRET_ACCESS_KEY_ENV])
        .ok_or_else(|| anyhow!("{R2_SECRET_ACCESS_KEY_ENV} must be set for R2 uploads"))?;
    let endpoint_url = env::var(R2_UPLOAD_ENDPOINT_ENV)
        .ok()
        .map(|value| trim_url_base(&value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("https://{account_id}.r2.cloudflarestorage.com"));
    Ok(R2Config {
        endpoint_url,
        bucket,
        access_key_id,
        secret_access_key,
    })
}

fn upload_file_to_r2(
    ctx: &TaskContext,
    config: &R2Config,
    local_path: &Path,
    public_url: &str,
) -> Result<()> {
    let object_key = url_path_key(public_url)?;
    let destination = format!("s3://{}/{}", config.bucket, object_key);
    let mut command = ctx.command("aws");
    command
        .env("AWS_ACCESS_KEY_ID", &config.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &config.secret_access_key)
        .env("AWS_DEFAULT_REGION", "auto")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .arg("s3")
        .arg("cp")
        .arg(local_path)
        .arg(destination)
        .arg("--endpoint-url")
        .arg(&config.endpoint_url)
        .arg("--content-type")
        .arg(content_type_for_upload(local_path))
        .arg("--only-show-errors");
    run_checked(&mut command)
}

fn upload_catalog_v3_artifact_urls_from_dir(
    ctx: &TaskContext,
    config: &R2Config,
    dir: &Path,
    artifact_url: &str,
    signature_url: &str,
) -> Result<bool> {
    let artifact_name = url_file_name(artifact_url)?;
    let artifact_path = dir.join(&artifact_name);
    if !artifact_path.is_file() {
        return Ok(false);
    }
    let signature_name = url_file_name(signature_url)?;
    let signature_path = dir.join(&signature_name);
    if !signature_path.is_file() {
        bail!("missing signature bundle {}", signature_path.display());
    }
    upload_file_to_r2(ctx, config, &artifact_path, artifact_url)?;
    upload_file_to_r2(ctx, config, &signature_path, signature_url)?;
    Ok(true)
}

fn upload_catalog_v3_artifact_from_dir(
    ctx: &TaskContext,
    config: &R2Config,
    dir: &Path,
    artifact: &CatalogV3Artifact,
) -> Result<bool> {
    upload_catalog_v3_artifact_urls_from_dir(
        ctx,
        config,
        dir,
        &artifact.url,
        &artifact.signature_url,
    )
}

fn upload_catalog_v3_plugin_artifact_from_dir(
    ctx: &TaskContext,
    config: &R2Config,
    dir: &Path,
    artifact: &CatalogV3PluginArtifact,
) -> Result<bool> {
    upload_catalog_v3_artifact_urls_from_dir(
        ctx,
        config,
        dir,
        &artifact.url,
        &artifact.signature_url,
    )
}

fn release_artifacts_are_present(dir: &Path, release: &CatalogV3Release) -> Result<bool> {
    for artifact in &release.artifacts {
        let artifact_name = url_file_name(&artifact.url)?;
        if !dir.join(artifact_name).is_file() {
            return Ok(false);
        }
        let signature_name = url_file_name(&artifact.signature_url)?;
        if !dir.join(signature_name).is_file() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn upload_redirected_catalog_from_dir(
    ctx: &TaskContext,
    config: &R2Config,
    dir: &Path,
    artifact: &CatalogV3RedirectArtifact,
) -> Result<()> {
    let catalog_name = url_file_name(&artifact.url)?;
    let catalog_path = dir.join(&catalog_name);
    if !catalog_path.is_file() {
        bail!(
            "missing redirected catalog artifact {}",
            catalog_path.display()
        );
    }
    upload_file_to_r2(ctx, config, &catalog_path, &artifact.url)?;
    let signature_name = url_file_name(&artifact.signature_url)?;
    let signature_path = dir.join(&signature_name);
    if !signature_path.is_file() {
        bail!(
            "missing redirected catalog signature bundle {}",
            signature_path.display()
        );
    }
    upload_file_to_r2(ctx, config, &signature_path, &artifact.signature_url)?;
    Ok(())
}

fn run_official_upload_r2(ctx: &TaskContext, dir: &Path) -> Result<()> {
    step(format!(
        "Uploading official plugin v3 assets to R2 from {}",
        dir.display()
    ));
    let config = r2_config_from_env()?;
    let snippet = read_prepared_catalog_v3_snippet(dir)?;
    let mut uploaded = 0usize;
    let mut matched_releases = 0usize;
    for release in &snippet.releases {
        if !release_artifacts_are_present(dir, release)? {
            continue;
        }
        matched_releases += 1;
        for artifact in &release.artifacts {
            if upload_catalog_v3_plugin_artifact_from_dir(ctx, &config, dir, artifact)? {
                uploaded += 1;
            }
        }
    }
    if matched_releases == 0 {
        bail!(
            "no prepared catalog-v3 release artifacts in {} matched snippet {}",
            dir.display(),
            snippet.id
        );
    }
    ok(format!("uploaded {uploaded} plugin artifact(s) to R2"));
    Ok(())
}

fn run_catalog_upload_v3_r2(ctx: &TaskContext, dir: &Path) -> Result<()> {
    step(format!(
        "Uploading catalog-v3 assets to R2 from {}",
        dir.display()
    ));
    let config = r2_config_from_env()?;
    let catalog = read_catalog_v3_from_path(ctx, &dir.join(CATALOG_V3_SNIPPET_JSON))?;
    let redirect_path = dir.join(CATALOG_V3_REDIRECT_JSON);
    let redirect: CatalogV3Redirect = serde_json::from_slice(&fs::read(&redirect_path)?)
        .with_context(|| format!("failed to parse {}", redirect_path.display()))?;

    for artifact in &redirect.artifacts {
        upload_redirected_catalog_from_dir(ctx, &config, dir, artifact)?;
    }

    let redirect_url = format!(
        "{}/{}/{}",
        public_catalog_base_url(),
        central_catalog_v3_path_prefix(),
        CATALOG_V3_REDIRECT_JSON
    );
    upload_file_to_r2(ctx, &config, &redirect_path, &redirect_url)?;
    let redirect_bundle_name = redirect_signature_bundle_file_name(CATALOG_V3_REDIRECT_JSON);
    let redirect_bundle_path = dir.join(&redirect_bundle_name);
    if !redirect_bundle_path.is_file() {
        bail!(
            "missing redirect signature bundle {}",
            redirect_bundle_path.display()
        );
    }
    let redirect_bundle_url = format!(
        "{}/{}/{}",
        public_catalog_base_url(),
        central_catalog_v3_path_prefix(),
        redirect_bundle_name
    );
    upload_file_to_r2(ctx, &config, &redirect_bundle_path, &redirect_bundle_url)?;

    let mut uploaded_rule_pack_artifacts = 0usize;
    for rule_pack in &catalog.rule_packs {
        for release in &rule_pack.releases {
            for artifact in &release.artifacts {
                if upload_catalog_v3_artifact_from_dir(ctx, &config, dir, artifact)? {
                    uploaded_rule_pack_artifacts += 1;
                }
            }
        }
    }

    ok(format!(
        "uploaded catalog redirects and {uploaded_rule_pack_artifacts} rule-pack artifact(s) to R2"
    ));
    Ok(())
}

fn run_community_scaffold(_ctx: &TaskContext, plugin_id: &str, output_dir: &Path) -> Result<()> {
    step(format!(
        "Scaffolding community plugin catalog for {plugin_id}"
    ));
    fs::create_dir_all(output_dir.join(".github/workflows"))?;
    fs::write(
        output_dir.join("catalog-v2.json"),
        serde_json::to_string_pretty(&ChildCatalogV2 {
            schema_version: CHILD_CATALOG_V2_SCHEMA.to_string(),
            id: plugin_id.to_string(),
            name: plugin_id.to_string(),
            description: "TODO: describe this plugin".to_string(),
            plugin_type: "indexer".to_string(),
            provider_type: plugin_id.to_string(),
            publisher: "TODO".to_string(),
            support_tier: "unverified".to_string(),
            docs_url: "https://github.com/OWNER/REPO".to_string(),
            source_repo: "https://github.com/OWNER/REPO".to_string(),
            releases: vec![ChildCatalogReleaseV2 {
                version: "0.1.0".to_string(),
                sdk_constraint: format!("^{SDK_VERSION}"),
                artifact_manifest_url:
                    "https://github.com/OWNER/REPO/releases/download/v0.1.0/plugin.manifest.json"
                        .to_string(),
            }],
        })? + "\n",
    )?;
    fs::write(
        output_dir.join(CATALOG_V3_SNIPPET_JSON),
        serde_json::to_string_pretty(&CatalogV3PluginEntry {
            id: plugin_id.to_string(),
            name: plugin_id.to_string(),
            description: "TODO: describe this plugin".to_string(),
            plugin_type: "indexer".to_string(),
            provider_type: plugin_id.to_string(),
            publisher: "TODO".to_string(),
            support_tier: "unverified".to_string(),
            status: PluginCatalogStatus::Active,
            docs_url: "https://github.com/OWNER/REPO".to_string(),
            source_repo: "https://github.com/OWNER/REPO".to_string(),
            required_signer: RequiredSignerV2 {
                github_repository: "OWNER/REPO".to_string(),
                github_workflow: Some(".github/workflows/release-plugin.yml".to_string()),
            },
            releases: vec![CatalogV3Release {
                version: "0.1.0".to_string(),
                sdk_constraint: format!("^{SDK_VERSION}"),
                min_scryer_version: None,
                artifacts: vec![CatalogV3PluginArtifact {
                    runtime: WASM_TARGET.to_string(),
                    required_features: Vec::new(),
                    wasm_digests: vec![
                        "blake3:REPLACE_ME".to_string(),
                        "shake256:REPLACE_ME".to_string(),
                    ],
                    bytes: 1,
                    url: "https://github.com/OWNER/REPO/releases/download/v0.1.0/plugin.wasm.zst"
                        .to_string(),
                    mirror_urls: Vec::new(),
                    signature_url:
                        "https://github.com/OWNER/REPO/releases/download/v0.1.0/plugin.wasm.zst.bundle.zst"
                            .to_string(),
                    signature_mirror_urls: Vec::new(),
                    digests: vec![
                        "blake3:REPLACE_ME".to_string(),
                        "shake256:REPLACE_ME".to_string(),
                    ],
                }],
            }],
        })? + "\n",
    )?;
    fs::write(
        output_dir.join(".github/workflows/release-plugin.yml"),
        "name: release-plugin\non:\n  push:\n    tags: ['v*']\npermissions:\n  contents: write\n  id-token: write\njobs:\n  build-sign-release:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - uses: sigstore/cosign-installer@v4.1.1\n        with:\n          cosign-release: v3.0.2\n      - run: echo 'Adapt this workflow to build wasm32-wasip1, run wasm-opt -Oz, compress with zstd -19 and brotli, compress v3 bundles to .bundle.zst, then publish catalog/v2 and catalog/v3 assets.'\n",
    )?;
    ok(format!("scaffolded {}", output_dir.display()));
    Ok(())
}

fn run_community_approve(_ctx: &TaskContext, github_repo: &str) -> Result<()> {
    bail!(
        "community approve is intentionally manual for now; add {github_repo} to catalog source and run community verify"
    )
}

fn parse_github_repo(value: &str) -> Result<String> {
    let trimmed = value.trim().trim_end_matches('/');
    let repo = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    let parts = repo.split('/').collect::<Vec<_>>();
    if parts.len() != 2 || parts.iter().any(|part| part.trim().is_empty()) {
        bail!("community repo must be owner/repo or a GitHub URL");
    }
    Ok(format!("{}/{}", parts[0], parts[1]))
}

fn regex_escape_literal(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' | '.' | '+' | '*' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            | '/' => vec!['\\', ch],
            _ => vec![ch],
        })
        .collect()
}

fn release_asset_url_parts(url: &str, expected_repo: &str) -> Result<(String, String)> {
    let prefix = format!("https://github.com/{expected_repo}/releases/download/");
    let rest = url
        .strip_prefix(&prefix)
        .ok_or_else(|| anyhow!("release asset URL must stay inside {expected_repo}: {url}"))?;
    let (tag, asset) = rest
        .split_once('/')
        .ok_or_else(|| anyhow!("release asset URL is missing an asset name: {url}"))?;
    if asset.contains('/') || asset.is_empty() {
        bail!("release asset URL has invalid asset name: {url}");
    }
    Ok((
        tag.replace("%2F", "/").replace("%2f", "/"),
        asset.to_string(),
    ))
}

fn github_release_download(
    ctx: &TaskContext,
    repo: &str,
    tag: &str,
    asset: &str,
    dir: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    run_checked(
        ctx.command("gh")
            .arg("release")
            .arg("download")
            .arg(tag)
            .arg("--repo")
            .arg(repo)
            .arg("--pattern")
            .arg(asset)
            .arg("--dir")
            .arg(dir)
            .arg("--clobber"),
    )?;
    let path = dir.join(asset);
    if !path.is_file() {
        bail!("gh did not download expected asset {}", path.display());
    }
    Ok(path)
}

fn github_release_download_if_exists(
    ctx: &TaskContext,
    repo: &str,
    tag: &str,
    asset: &str,
    dir: &Path,
) -> Result<Option<PathBuf>> {
    fs::create_dir_all(dir)?;
    let status = run_status(
        ctx.command("gh")
            .arg("release")
            .arg("download")
            .arg(tag)
            .arg("--repo")
            .arg(repo)
            .arg("--pattern")
            .arg(asset)
            .arg("--dir")
            .arg(dir)
            .arg("--clobber"),
    )?;
    let path = dir.join(asset);
    if status.success() && path.is_file() {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

fn cosign_verify_blob_with_identity_pattern(
    ctx: &TaskContext,
    blob: &Path,
    bundle: &Path,
    identity_pattern: &str,
) -> Result<()> {
    let temp_bundle = if matches!(
        bundle.extension().and_then(OsStr::to_str),
        Some("zst" | "br")
    ) {
        let temp_bundle = tempfile::NamedTempFile::new()?;
        let bundle_bytes = read_catalog_bytes(ctx, bundle)?;
        fs::write(temp_bundle.path(), bundle_bytes)?;
        Some(temp_bundle)
    } else {
        None
    };
    let bundle_path = temp_bundle.as_ref().map_or(bundle, |temp| temp.path());
    run_checked(
        ctx.command("cosign")
            .arg("verify-blob")
            .arg("--bundle")
            .arg(bundle_path)
            .arg("--certificate-identity-regexp")
            .arg(identity_pattern)
            .arg("--certificate-oidc-issuer")
            .arg("https://token.actions.githubusercontent.com")
            .arg(blob),
    )
    .with_context(|| {
        format!(
            "failed to run cosign verify-blob for {} (ensure cosign is installed and on PATH)",
            blob.display()
        )
    })
}

fn cosign_verify_blob(ctx: &TaskContext, repo: &str, blob: &Path, bundle: &Path) -> Result<()> {
    let identity_pattern = format!(
        "^https://github\\.com/{}/\\.github/workflows/.*@refs/(tags|heads)/.*$",
        regex_escape_literal(repo)
    );
    cosign_verify_blob_with_identity_pattern(ctx, blob, bundle, &identity_pattern)
}

fn cosign_verify_official_blob(ctx: &TaskContext, blob: &Path, bundle: &Path) -> Result<()> {
    let identity_pattern = format!(
        "^https://github\\.com/{}/{}@refs/(tags|heads)/.*$",
        regex_escape_literal(OFFICIAL_GITHUB_REPO),
        regex_escape_literal(&official_release_workflow()),
    );
    cosign_verify_blob_with_identity_pattern(ctx, blob, bundle, &identity_pattern)
}

fn require_blake3_file(label: &str, expected: &str, path: &Path) -> Result<()> {
    let actual = blake3_file(path)?;
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("{label} digest mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn validate_community_child_catalog(catalog: &ChildCatalogV2, repo: &str) -> Result<()> {
    if catalog.schema_version != CHILD_CATALOG_V2_SCHEMA {
        bail!(
            "unsupported child catalog schema {}",
            catalog.schema_version
        );
    }
    for (label, value) in [
        ("id", &catalog.id),
        ("name", &catalog.name),
        ("plugin_type", &catalog.plugin_type),
        ("provider_type", &catalog.provider_type),
        ("publisher", &catalog.publisher),
        ("docs_url", &catalog.docs_url),
        ("source_repo", &catalog.source_repo),
    ] {
        if value.trim().is_empty() {
            bail!("child catalog field {label} is required");
        }
    }
    if catalog.support_tier != "verified_community" && catalog.support_tier != "unverified" {
        bail!(
            "community child catalog cannot self-declare support tier {}",
            catalog.support_tier
        );
    }
    if !catalog.source_repo.contains(repo) {
        bail!(
            "child catalog source_repo must reference {repo}: {}",
            catalog.source_repo
        );
    }

    let mut versions = std::collections::HashSet::new();
    for release in &catalog.releases {
        Version::parse(&release.version)
            .with_context(|| format!("invalid release version {}", release.version))?;
        semver::VersionReq::parse(&release.sdk_constraint)
            .with_context(|| format!("invalid SDK constraint {}", release.sdk_constraint))?;
        if !versions.insert(release.version.clone()) {
            bail!("duplicate child release version {}", release.version);
        }
        release_asset_url_parts(&release.artifact_manifest_url, repo)?;
    }
    Ok(())
}

fn latest_child_catalog_release(catalog: &ChildCatalogV2) -> Result<&ChildCatalogReleaseV2> {
    catalog
        .releases
        .iter()
        .max_by(|left, right| {
            Version::parse(&left.version)
                .ok()
                .cmp(&Version::parse(&right.version).ok())
        })
        .ok_or_else(|| anyhow!("{} child catalog has no releases", catalog.id))
}

fn validate_release_manifest(
    manifest: &PluginManifestV2,
    child: &ChildCatalogV2,
    release: &ChildCatalogReleaseV2,
) -> Result<()> {
    if manifest.schema_version != PLUGIN_MANIFEST_SCHEMA {
        bail!(
            "unsupported plugin manifest schema {}",
            manifest.schema_version
        );
    }
    if manifest.id != child.id
        || manifest.plugin_type != child.plugin_type
        || manifest.provider_type != child.provider_type
        || manifest.version != release.version
        || manifest.publisher != child.publisher
    {
        bail!("plugin manifest identity does not match child catalog release");
    }
    if manifest.artifact != "plugin.wasm.zst" {
        bail!("plugin manifest artifact must be plugin.wasm.zst");
    }
    if manifest.compression != "zstd" {
        bail!("plugin manifest compression must be zstd");
    }
    if manifest.signature != "plugin.wasm.zst.bundle" {
        bail!("plugin manifest signature must be plugin.wasm.zst.bundle");
    }
    for (label, digest) in [
        ("wasm_digest", &manifest.wasm_digest),
        ("artifact_digest", &manifest.artifact_digest),
    ] {
        let Some(hex) = digest.strip_prefix("blake3:") else {
            bail!("{label} must be a blake3:<hex> digest");
        };
        if hex.len() != 64 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            bail!("{label} must contain a 64-character hex BLAKE3 digest");
        }
    }
    Ok(())
}

fn catalog_v3_url_uses_v3_lane(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed.contains("/plugins-v3/") || trimmed.contains("/download/plugins-v3%2F")
}

fn validate_catalog_v3_plugin_entry(plugin: &CatalogV3PluginEntry) -> Result<()> {
    for (label, value) in [
        ("id", &plugin.id),
        ("name", &plugin.name),
        ("plugin_type", &plugin.plugin_type),
        ("provider_type", &plugin.provider_type),
        ("publisher", &plugin.publisher),
        ("support_tier", &plugin.support_tier),
        ("docs_url", &plugin.docs_url),
        ("source_repo", &plugin.source_repo),
    ] {
        if value.trim().is_empty() {
            bail!("catalog-v3 plugin field {label} is required");
        }
    }
    if plugin.releases.is_empty() {
        bail!(
            "{}: catalog-v3 plugin must include at least one release",
            plugin.id
        );
    }
    if plugin.required_signer.github_repository.trim().is_empty() {
        bail!(
            "{}: catalog-v3 plugin required_signer.github_repository is required",
            plugin.id
        );
    }

    let mut versions = BTreeSet::new();
    for release in &plugin.releases {
        Version::parse(&release.version).with_context(|| {
            format!("{}: invalid release version {}", plugin.id, release.version)
        })?;
        semver::VersionReq::parse(&release.sdk_constraint).with_context(|| {
            format!(
                "{} {}: invalid SDK constraint {}",
                plugin.id, release.version, release.sdk_constraint
            )
        })?;
        if let Some(min_scryer_version) = release.min_scryer_version.as_deref() {
            Version::parse(min_scryer_version.trim()).with_context(|| {
                format!(
                    "{} {}: invalid min_scryer_version {}",
                    plugin.id, release.version, min_scryer_version
                )
            })?;
        }
        if !versions.insert(release.version.clone()) {
            bail!(
                "{}: duplicate catalog-v3 release {}",
                plugin.id,
                release.version
            );
        }
        if release.artifacts.is_empty() {
            bail!(
                "{} {}: at least one artifact is required",
                plugin.id,
                release.version
            );
        }
        let mut artifact_variants = BTreeSet::new();
        for artifact in &release.artifacts {
            if artifact.runtime.trim() != WASM_TARGET {
                bail!(
                    "{} {}: artifact runtime must be {}",
                    plugin.id,
                    release.version,
                    WASM_TARGET
                );
            }
            let normalized_required_features_set =
                WasmFeatureSet::new(artifact.required_features.clone());
            normalized_required_features_set.validate()?;
            let normalized_required_features = normalized_required_features_set.required_features;
            if normalized_required_features != artifact.required_features {
                bail!(
                    "{} {}: artifact required_features must be sorted and deduplicated",
                    plugin.id,
                    release.version
                );
            }
            if artifact.url.trim().is_empty() {
                bail!(
                    "{} {}: artifact url is required",
                    plugin.id,
                    release.version
                );
            }
            if artifact.signature_url.trim().is_empty() {
                bail!(
                    "{} {}: artifact signature_url is required",
                    plugin.id,
                    release.version
                );
            }
            for (label, url) in [
                ("artifact url", &artifact.url),
                ("artifact signature_url", &artifact.signature_url),
            ] {
                if !catalog_v3_url_uses_v3_lane(url) {
                    bail!(
                        "{} {}: catalog-v3 {label} must use the plugins-v3 lane: {}",
                        plugin.id,
                        release.version,
                        url
                    );
                }
            }
            if artifact.digests.is_empty() {
                bail!(
                    "{} {}: artifact digests are required",
                    plugin.id,
                    release.version
                );
            }
            if artifact.wasm_digests.is_empty() {
                bail!(
                    "{} {}: artifact wasm_digests are required",
                    plugin.id,
                    release.version
                );
            }
            for mirror_url in &artifact.mirror_urls {
                if mirror_url.trim().is_empty() {
                    bail!(
                        "{} {}: artifact mirror_urls must not contain empty entries",
                        plugin.id,
                        release.version
                    );
                }
                if !catalog_v3_url_uses_v3_lane(mirror_url) {
                    bail!(
                        "{} {}: catalog-v3 artifact mirror_url must use the plugins-v3 lane: {}",
                        plugin.id,
                        release.version,
                        mirror_url
                    );
                }
            }
            for mirror_url in &artifact.signature_mirror_urls {
                if mirror_url.trim().is_empty() {
                    bail!(
                        "{} {}: artifact signature_mirror_urls must not contain empty entries",
                        plugin.id,
                        release.version
                    );
                }
                if !catalog_v3_url_uses_v3_lane(mirror_url) {
                    bail!(
                        "{} {}: catalog-v3 artifact signature_mirror_url must use the plugins-v3 lane: {}",
                        plugin.id,
                        release.version,
                        mirror_url
                    );
                }
            }
            for digest in &artifact.digests {
                validate_digest_string("artifact digests", digest)?;
            }
            for digest in &artifact.wasm_digests {
                validate_digest_string("artifact wasm_digests", digest)?;
            }
            if artifact.bytes == 0 {
                bail!(
                    "{} {}: artifact bytes must be greater than zero",
                    plugin.id,
                    release.version
                );
            }
            let artifact_name = url_file_name(&artifact.url)?;
            if !(artifact_name.ends_with(".wasm.zst") || artifact_name.ends_with(".wasm.br")) {
                bail!(
                    "{} {}: plugin artifact {} must end with .wasm.zst or .wasm.br",
                    plugin.id,
                    release.version,
                    artifact_name
                );
            }
            let encoding = if artifact_name.ends_with(".wasm.zst") {
                "zst"
            } else {
                "br"
            };
            if !artifact_variants.insert((
                artifact.runtime.clone(),
                artifact.required_features.clone(),
                encoding.to_string(),
            )) {
                bail!(
                    "{} {}: duplicate artifact variant for runtime {}, required_features {:?}, encoding {}",
                    plugin.id,
                    release.version,
                    artifact.runtime,
                    artifact.required_features,
                    encoding
                );
            }
        }
    }
    Ok(())
}

fn validate_catalog_v3_release_artifact(
    artifact: &CatalogV3PluginArtifact,
    artifact_path: &Path,
    wasm_path: &Path,
) -> Result<()> {
    let actual_artifact_digests = file_digests(artifact_path)?;
    if artifact.digests != actual_artifact_digests {
        bail!(
            "{}: artifact digests do not match downloaded asset",
            artifact.url
        );
    }

    let actual_wasm_digests = file_digests(wasm_path)?;
    if artifact.wasm_digests != actual_wasm_digests {
        bail!(
            "{}: wasm_digests do not match decompressed artifact payload",
            artifact.url
        );
    }
    let actual_bytes = fs::metadata(wasm_path)
        .with_context(|| format!("failed to stat {}", wasm_path.display()))?
        .len();
    if actual_bytes != artifact.bytes {
        bail!(
            "{}: bytes does not match decompressed artifact payload",
            artifact.url
        );
    }

    Ok(())
}

fn snippet_artifacts_are_missing(
    artifacts: &[CatalogV3PluginArtifact],
    dir: &Path,
) -> Result<bool> {
    for artifact in artifacts {
        let artifact_name = url_file_name(&artifact.url)?;
        if !dir.join(artifact_name).is_file() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_catalog_v3(catalog: &CatalogV3) -> Result<()> {
    if catalog.schema_version != CATALOG_V3_SCHEMA {
        bail!(
            "unsupported central catalog-v3 schema {}",
            catalog.schema_version
        );
    }

    let mut plugin_ids = BTreeSet::new();
    for plugin in &catalog.plugins {
        if !plugin_ids.insert(plugin.id.clone()) {
            bail!("duplicate official plugin id {}", plugin.id);
        }
        if plugin.publisher != "scryer" {
            bail!("{}: publisher must be scryer", plugin.id);
        }
        if plugin.support_tier != "official" {
            bail!("{}: support_tier must be official", plugin.id);
        }
        if plugin.required_signer.github_repository != OFFICIAL_GITHUB_REPO {
            bail!(
                "{}: required_signer.github_repository must be {}",
                plugin.id,
                OFFICIAL_GITHUB_REPO
            );
        }
        validate_catalog_v3_plugin_entry(plugin)?;
    }

    let mut rule_pack_ids = BTreeSet::new();
    for rule_pack in &catalog.rule_packs {
        if !rule_pack_ids.insert(rule_pack.id.clone()) {
            bail!("duplicate rule pack id {}", rule_pack.id);
        }
        for (label, value) in [
            ("id", &rule_pack.id),
            ("name", &rule_pack.name),
            ("description", &rule_pack.description),
            ("author", &rule_pack.author),
        ] {
            if value.trim().is_empty() {
                bail!("catalog-v3 rule pack field {label} is required");
            }
        }
        if rule_pack.releases.is_empty() {
            bail!(
                "{}: catalog-v3 rule pack must include at least one release",
                rule_pack.id
            );
        }
        let mut release_versions = BTreeSet::new();
        for release in &rule_pack.releases {
            Version::parse(&release.version).with_context(|| {
                format!(
                    "{}: invalid rule pack release version {}",
                    rule_pack.id, release.version
                )
            })?;
            if !release_versions.insert(release.version.clone()) {
                bail!(
                    "{}: duplicate rule pack release {}",
                    rule_pack.id,
                    release.version
                );
            }
            if let Some(min_scryer_version) = release.min_scryer_version.as_deref() {
                Version::parse(min_scryer_version).with_context(|| {
                    format!(
                        "{} {}: invalid min_scryer_version {}",
                        rule_pack.id, release.version, min_scryer_version
                    )
                })?;
            }
            if release.rule_pack_digests.is_empty() {
                bail!(
                    "{} {}: rule_pack_digests are required",
                    rule_pack.id,
                    release.version
                );
            }
            for digest in &release.rule_pack_digests {
                validate_digest_string("rule_pack_digests", digest)?;
            }
            if release.artifacts.is_empty() {
                bail!(
                    "{} {}: rule pack artifacts are required",
                    rule_pack.id,
                    release.version
                );
            }
            for artifact in &release.artifacts {
                let artifact_name = url_file_name(&artifact.url)?;
                if !(artifact_name.ends_with(".min.json.zst")
                    || artifact_name.ends_with(".min.json.br"))
                {
                    bail!(
                        "{} {}: rule pack artifact {} must end with .min.json.zst or .min.json.br",
                        rule_pack.id,
                        release.version,
                        artifact_name
                    );
                }
                if artifact.signature_url.trim().is_empty() {
                    bail!(
                        "{} {}: rule pack artifact signature_url is required",
                        rule_pack.id,
                        release.version
                    );
                }
                if artifact.digests.is_empty() {
                    bail!(
                        "{} {}: rule pack artifact digests are required",
                        rule_pack.id,
                        release.version
                    );
                }
            }
        }
    }

    Ok(())
}

fn validate_official_catalog(catalog: &CatalogV2) -> Result<()> {
    if catalog.schema_version != CATALOG_V2_SCHEMA {
        bail!(
            "unsupported central catalog schema {}",
            catalog.schema_version
        );
    }

    let mut plugin_ids = BTreeSet::new();
    for plugin in &catalog.plugins {
        for (label, value) in [
            ("id", &plugin.id),
            ("name", &plugin.name),
            ("plugin_type", &plugin.plugin_type),
            ("provider_type", &plugin.provider_type),
            ("publisher", &plugin.publisher),
            ("docs_url", &plugin.docs_url),
            ("source_repo", &plugin.source_repo),
            ("child_catalog_url", &plugin.child_catalog_url),
        ] {
            if value.trim().is_empty() {
                bail!("catalog-v2 plugin field {label} is required");
            }
        }
        if !plugin_ids.insert(plugin.id.clone()) {
            bail!("duplicate official plugin id {}", plugin.id);
        }
        if plugin.publisher != "scryer" {
            bail!("{}: publisher must be scryer", plugin.id);
        }
        if plugin.support_tier != "official" {
            bail!("{}: support_tier must be official", plugin.id);
        }
        if plugin.required_signer.github_repository != OFFICIAL_GITHUB_REPO {
            bail!(
                "{}: required_signer.github_repository must be {}",
                plugin.id,
                OFFICIAL_GITHUB_REPO
            );
        }
        let expected_workflow = official_release_workflow();
        let default_workflow = DEFAULT_OFFICIAL_RELEASE_WORKFLOW;
        let signer_workflow = plugin.required_signer.github_workflow.as_deref();
        if signer_workflow != Some(expected_workflow.as_str())
            && signer_workflow != Some(default_workflow)
        {
            bail!(
                "{}: required_signer.github_workflow must be {}",
                plugin.id,
                expected_workflow
            );
        }

        let (_, asset) = release_asset_url_parts(&plugin.child_catalog_url, OFFICIAL_GITHUB_REPO)?;
        if asset != CATALOG_MINIFIED_ZST {
            bail!(
                "{}: child_catalog_url must point at {}",
                plugin.id,
                CATALOG_MINIFIED_ZST
            );
        }
    }

    let mut rule_pack_ids = BTreeSet::new();
    for rule_pack in &catalog.rule_packs {
        for (label, value) in [
            ("id", &rule_pack.id),
            ("name", &rule_pack.name),
            ("author", &rule_pack.author),
            ("version", &rule_pack.version),
            ("url", &rule_pack.url),
        ] {
            if value.trim().is_empty() {
                bail!("catalog-v2 rule pack field {label} is required");
            }
        }
        if !rule_pack_ids.insert(rule_pack.id.clone()) {
            bail!("duplicate official rule pack id {}", rule_pack.id);
        }
        Version::parse(rule_pack.version.trim()).with_context(|| {
            format!(
                "{}: invalid rule pack version {}",
                rule_pack.id, rule_pack.version
            )
        })?;
        if let Some(min_scryer_version) = rule_pack.min_scryer_version.as_deref() {
            Version::parse(min_scryer_version.trim()).with_context(|| {
                format!(
                    "{}: invalid min_scryer_version {}",
                    rule_pack.id, min_scryer_version
                )
            })?;
        }
        let (tag, asset) = release_asset_url_parts(&rule_pack.url, OFFICIAL_GITHUB_REPO)?;
        if tag != CENTRAL_CATALOG_RELEASE_TAG {
            bail!(
                "{}: rule pack asset must be published on {}",
                rule_pack.id,
                CENTRAL_CATALOG_RELEASE_TAG
            );
        }
        if asset.trim().is_empty() {
            bail!(
                "{}: rule pack asset URL must include a filename",
                rule_pack.id
            );
        }
    }

    Ok(())
}

fn validate_official_child_catalog(
    catalog: &ChildCatalogV2,
    central_entry: &CatalogV2Entry,
) -> Result<()> {
    if catalog.schema_version != CHILD_CATALOG_V2_SCHEMA {
        bail!(
            "{}: unsupported child catalog schema {}",
            catalog.id,
            catalog.schema_version
        );
    }
    for (label, child_value, central_value) in [
        ("id", &catalog.id, &central_entry.id),
        ("name", &catalog.name, &central_entry.name),
        (
            "description",
            &catalog.description,
            &central_entry.description,
        ),
        (
            "plugin_type",
            &catalog.plugin_type,
            &central_entry.plugin_type,
        ),
        (
            "provider_type",
            &catalog.provider_type,
            &central_entry.provider_type,
        ),
        ("publisher", &catalog.publisher, &central_entry.publisher),
        (
            "support_tier",
            &catalog.support_tier,
            &central_entry.support_tier,
        ),
        ("docs_url", &catalog.docs_url, &central_entry.docs_url),
        (
            "source_repo",
            &catalog.source_repo,
            &central_entry.source_repo,
        ),
    ] {
        if child_value != central_value {
            bail!(
                "{}: child catalog {label}={} does not match central catalog {label}={}",
                catalog.id,
                child_value,
                central_value
            );
        }
    }

    let mut versions = BTreeSet::new();
    for release in &catalog.releases {
        Version::parse(&release.version).with_context(|| {
            format!(
                "{}: invalid release version {}",
                catalog.id, release.version
            )
        })?;
        semver::VersionReq::parse(&release.sdk_constraint).with_context(|| {
            format!(
                "{} {}: invalid SDK constraint {}",
                catalog.id, release.version, release.sdk_constraint
            )
        })?;
        let supported = catalog_v2_supported_sdk_constraint(&release.sdk_constraint)?;
        if !supported {
            bail!(
                "{} {}: official child catalog release predates the catalog-v2 base SDK {}",
                catalog.id,
                release.version,
                CATALOG_V2_BASE_SDK_VERSION
            );
        }
        if !versions.insert(release.version.clone()) {
            bail!(
                "{}: duplicate child release version {}",
                catalog.id,
                release.version
            );
        }
        release_asset_url_parts(&release.artifact_manifest_url, OFFICIAL_GITHUB_REPO)?;
    }

    let latest_release = latest_child_catalog_release(catalog)?;
    let latest_supported =
        host_version_matches_constraint(SDK_VERSION, &latest_release.sdk_constraint)
            .map_err(anyhow::Error::msg)?;
    if !latest_supported {
        bail!(
            "{} {}: latest official child catalog release is not compatible with host SDK {}",
            catalog.id,
            latest_release.version,
            SDK_VERSION
        );
    }

    Ok(())
}

fn validate_official_release_descriptor(
    descriptor: &PluginDescriptor,
    child: &ChildCatalogV2,
    release: &ChildCatalogReleaseV2,
) -> Result<()> {
    validate_descriptor_contract(descriptor)?;
    let expected = [
        ("id", child.id.as_str(), descriptor.id.as_str()),
        (
            "version",
            release.version.as_str(),
            descriptor.version.as_str(),
        ),
        (
            "plugin_type",
            child.plugin_type.as_str(),
            descriptor.plugin_type(),
        ),
        (
            "provider_type",
            child.provider_type.as_str(),
            descriptor.provider_type(),
        ),
    ];
    for (field, expected_value, actual_value) in expected {
        if expected_value != actual_value {
            bail!(
                "{}: latest release descriptor {field}={} does not match published {field}={}",
                child.id,
                actual_value,
                expected_value
            );
        }
    }
    Ok(())
}

fn run_community_verify(ctx: &TaskContext, github_repo: &str) -> Result<()> {
    let repo = parse_github_repo(github_repo)?;
    step(format!("Verifying community repo {repo}"));

    let temp = tempfile::tempdir()?;
    let v3_dir = temp.path().join("catalog-v3");
    let v3_snippet = github_release_download_if_exists(
        ctx,
        &repo,
        "catalog/v3",
        CATALOG_V3_SNIPPET_JSON,
        &v3_dir,
    )?;
    if let Some(v3_snippet) = v3_snippet {
        let v3_bundle = github_release_download(
            ctx,
            &repo,
            "catalog/v3",
            &format!("{CATALOG_V3_SNIPPET_JSON}.bundle.zst"),
            &v3_dir,
        )?;
        cosign_verify_blob(ctx, &repo, &v3_snippet, &v3_bundle)?;
        let plugin = read_catalog_v3_snippet_from_path(&v3_snippet)?;
        validate_catalog_v3_plugin_entry(&plugin)?;
        if plugin.required_signer.github_repository != repo {
            bail!("catalog-v3 snippet required_signer.github_repository must reference {repo}");
        }
        if !plugin.source_repo.contains(&repo) {
            bail!(
                "catalog-v3 snippet source_repo must reference {repo}: {}",
                plugin.source_repo
            );
        }

        for release in &plugin.releases {
            let release_dir = temp.path().join(&plugin.id).join(&release.version);
            for (index, artifact) in release.artifacts.iter().enumerate() {
                let (artifact_tag, artifact_name) = release_asset_url_parts(&artifact.url, &repo)?;
                let (_, signature_name) = release_asset_url_parts(&artifact.signature_url, &repo)?;
                let artifact_path = github_release_download(
                    ctx,
                    &repo,
                    &artifact_tag,
                    &artifact_name,
                    &release_dir,
                )?;
                let signature_path = github_release_download(
                    ctx,
                    &repo,
                    &artifact_tag,
                    &signature_name,
                    &release_dir,
                )?;
                cosign_verify_blob(ctx, &repo, &artifact_path, &signature_path)?;
                let wasm = release_dir.join(format!("plugin-{index}.wasm"));
                decompress_plugin_wasm_artifact(ctx, &artifact_path, &wasm)?;
                validate_catalog_v3_release_artifact(artifact, &artifact_path, &wasm)
                    .with_context(|| {
                        format!(
                            "{} {}: artifact verification failed for {}",
                            plugin.id, release.version, artifact.url
                        )
                    })?;
            }
        }

        ok(format!(
            "verified {} release(s) for {} via catalog-v3",
            plugin.releases.len(),
            repo
        ));
        return Ok(());
    }

    let catalog_dir = temp.path().join("catalog");
    let catalog =
        github_release_download(ctx, &repo, "catalog/v2", CATALOG_MINIFIED_ZST, &catalog_dir)?;
    let catalog_bundle = github_release_download(
        ctx,
        &repo,
        "catalog/v2",
        &format!("{CATALOG_MINIFIED_ZST}.bundle"),
        &catalog_dir,
    )?;
    cosign_verify_blob(ctx, &repo, &catalog, &catalog_bundle)?;

    let child: ChildCatalogV2 = serde_json::from_slice(&read_catalog_bytes(ctx, &catalog)?)
        .with_context(|| format!("failed to parse {}", catalog.display()))?;
    validate_community_child_catalog(&child, &repo)?;

    for release in &child.releases {
        let (tag, manifest_asset) = release_asset_url_parts(&release.artifact_manifest_url, &repo)?;
        let release_dir = temp.path().join(&child.id).join(&release.version);
        let manifest_path =
            github_release_download(ctx, &repo, &tag, &manifest_asset, &release_dir)?;
        let manifest_bundle = github_release_download(
            ctx,
            &repo,
            &tag,
            &format!("{manifest_asset}.bundle"),
            &release_dir,
        )?;
        cosign_verify_blob(ctx, &repo, &manifest_path, &manifest_bundle)?;

        let manifest: PluginManifestV2 = serde_json::from_slice(&fs::read(&manifest_path)?)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        validate_release_manifest(&manifest, &child, release)?;

        let artifact = github_release_download(ctx, &repo, &tag, &manifest.artifact, &release_dir)?;
        let artifact_bundle =
            github_release_download(ctx, &repo, &tag, &manifest.signature, &release_dir)?;
        cosign_verify_blob(ctx, &repo, &artifact, &artifact_bundle)?;
        require_blake3_file("compressed artifact", &manifest.artifact_digest, &artifact)?;

        let wasm = release_dir.join("plugin.wasm");
        run_checked(
            ctx.command("zstd")
                .arg("-d")
                .arg("-f")
                .arg(&artifact)
                .arg("-o")
                .arg(&wasm),
        )?;
        require_blake3_file("decompressed WASM", &manifest.wasm_digest, &wasm)?;
    }

    ok(format!(
        "verified {} release(s) for {}",
        child.releases.len(),
        repo
    ));
    Ok(())
}

fn run_catalog_validate_v2(ctx: &TaskContext) -> Result<()> {
    step("Validating published official catalog-v2 assets");

    let temp = tempfile::tempdir()?;
    let catalog_dir = temp.path().join("catalog");
    let central_pretty = github_release_download(
        ctx,
        OFFICIAL_GITHUB_REPO,
        CENTRAL_CATALOG_RELEASE_TAG,
        CATALOG_PRETTY_JSON,
        &catalog_dir,
    )?;
    let central_pretty_bundle = github_release_download(
        ctx,
        OFFICIAL_GITHUB_REPO,
        CENTRAL_CATALOG_RELEASE_TAG,
        &format!("{CATALOG_PRETTY_JSON}.bundle"),
        &catalog_dir,
    )?;
    let central_runtime = github_release_download(
        ctx,
        OFFICIAL_GITHUB_REPO,
        CENTRAL_CATALOG_RELEASE_TAG,
        CATALOG_MINIFIED_ZST,
        &catalog_dir,
    )?;
    let central_runtime_bundle = github_release_download(
        ctx,
        OFFICIAL_GITHUB_REPO,
        CENTRAL_CATALOG_RELEASE_TAG,
        &format!("{CATALOG_MINIFIED_ZST}.bundle"),
        &catalog_dir,
    )?;
    cosign_verify_official_blob(ctx, &central_pretty, &central_pretty_bundle)?;
    cosign_verify_official_blob(ctx, &central_runtime, &central_runtime_bundle)?;

    let pretty_value: serde_json::Value = serde_json::from_slice(&fs::read(&central_pretty)?)
        .with_context(|| format!("failed to parse {}", central_pretty.display()))?;
    let runtime_value: serde_json::Value =
        serde_json::from_slice(&read_catalog_bytes(ctx, &central_runtime)?)
            .with_context(|| format!("failed to parse {}", central_runtime.display()))?;
    if pretty_value != runtime_value {
        bail!("published catalog-v2 pretty JSON and zstd runtime asset differ");
    }
    let catalog: CatalogV2 = serde_json::from_value(runtime_value)?;
    validate_official_catalog(&catalog)?;

    for rule_pack in &catalog.rule_packs {
        let (tag, asset) = release_asset_url_parts(&rule_pack.url, OFFICIAL_GITHUB_REPO)?;
        let pack_dir = temp.path().join("rule-packs");
        let path = github_release_download(ctx, OFFICIAL_GITHUB_REPO, &tag, &asset, &pack_dir)?;
        let manifest = load_rule_pack_manifest(&path)?;
        if manifest.id != rule_pack.id
            || manifest.name != rule_pack.name
            || manifest.author != rule_pack.author
            || manifest.version != rule_pack.version
        {
            bail!(
                "{}: published rule pack asset does not match central catalog metadata",
                rule_pack.id
            );
        }
    }

    for plugin in &catalog.plugins {
        let (tag, asset) =
            release_asset_url_parts(&plugin.child_catalog_url, OFFICIAL_GITHUB_REPO)?;
        let child_dir = temp.path().join("plugins").join(&plugin.id);
        let child_pretty = github_release_download(
            ctx,
            OFFICIAL_GITHUB_REPO,
            &tag,
            CATALOG_PRETTY_JSON,
            &child_dir,
        )?;
        let child_pretty_bundle = github_release_download(
            ctx,
            OFFICIAL_GITHUB_REPO,
            &tag,
            &format!("{CATALOG_PRETTY_JSON}.bundle"),
            &child_dir,
        )?;
        let child_runtime =
            github_release_download(ctx, OFFICIAL_GITHUB_REPO, &tag, &asset, &child_dir)?;
        let child_runtime_bundle = github_release_download(
            ctx,
            OFFICIAL_GITHUB_REPO,
            &tag,
            &format!("{asset}.bundle"),
            &child_dir,
        )?;
        cosign_verify_official_blob(ctx, &child_pretty, &child_pretty_bundle)?;
        cosign_verify_official_blob(ctx, &child_runtime, &child_runtime_bundle)?;

        let child_pretty_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&child_pretty)?)
                .with_context(|| format!("failed to parse {}", child_pretty.display()))?;
        let child_runtime_value: serde_json::Value =
            serde_json::from_slice(&read_catalog_bytes(ctx, &child_runtime)?)
                .with_context(|| format!("failed to parse {}", child_runtime.display()))?;
        if child_pretty_value != child_runtime_value {
            bail!(
                "{}: child catalog pretty JSON and zstd runtime asset differ",
                plugin.id
            );
        }

        let child: ChildCatalogV2 = serde_json::from_value(child_runtime_value)?;
        validate_official_child_catalog(&child, plugin)?;
        let latest_release = latest_child_catalog_release(&child)?;
        let (release_tag, manifest_asset) =
            release_asset_url_parts(&latest_release.artifact_manifest_url, OFFICIAL_GITHUB_REPO)?;
        let release_dir = temp.path().join(&child.id).join(&latest_release.version);
        let manifest_path = github_release_download(
            ctx,
            OFFICIAL_GITHUB_REPO,
            &release_tag,
            &manifest_asset,
            &release_dir,
        )?;
        let manifest_bundle = github_release_download(
            ctx,
            OFFICIAL_GITHUB_REPO,
            &release_tag,
            &format!("{manifest_asset}.bundle"),
            &release_dir,
        )?;
        cosign_verify_official_blob(ctx, &manifest_path, &manifest_bundle)?;

        let manifest: PluginManifestV2 = serde_json::from_slice(&fs::read(&manifest_path)?)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        validate_release_manifest(&manifest, &child, latest_release)?;

        let artifact = github_release_download(
            ctx,
            OFFICIAL_GITHUB_REPO,
            &release_tag,
            &manifest.artifact,
            &release_dir,
        )?;
        let artifact_bundle = github_release_download(
            ctx,
            OFFICIAL_GITHUB_REPO,
            &release_tag,
            &manifest.signature,
            &release_dir,
        )?;
        cosign_verify_official_blob(ctx, &artifact, &artifact_bundle)?;
        require_blake3_file("compressed artifact", &manifest.artifact_digest, &artifact)?;

        let wasm = release_dir.join("plugin.wasm");
        run_checked(
            ctx.command("zstd")
                .arg("-d")
                .arg("-f")
                .arg(&artifact)
                .arg("-o")
                .arg(&wasm),
        )?;
        require_blake3_file("decompressed WASM", &manifest.wasm_digest, &wasm)?;

        let descriptor = load_descriptor_from_wasm(&wasm)?;
        validate_official_release_descriptor(&descriptor, &child, latest_release)?;
    }

    ok(format!(
        "verified published official catalog-v2 for {} plugin(s)",
        catalog.plugins.len()
    ));
    Ok(())
}

fn run_ffmpeg_revendor(ctx: &TaskContext, args: FfmpegRevendorArgs) -> Result<()> {
    step(format!(
        "Re-vendoring FFmpeg {} into enhanced subtitle sync",
        args.commit
    ));

    let scratch = tempfile::tempdir().context("create FFmpeg re-vendor scratch directory")?;
    let source = prepare_ffmpeg_source(ctx, &args, scratch.path())?;
    let commit = ensure_ffmpeg_commit(ctx, &source, &args.commit)?;
    let source_date = git_capture_in(
        ctx,
        &source,
        ["show", "-s", "--format=%cs", commit.as_str()],
    )
    .context("read FFmpeg commit date")?;
    let source_url = git_capture_in(ctx, &source, ["config", "--get", "remote.origin.url"])
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|_| args.source.clone());

    let staged_vendor = scratch.path().join("ffmpeg-vendor");
    fs::create_dir_all(&staged_vendor).context("create staged FFmpeg vendor directory")?;
    write_ffmpeg_vendor_archive(ctx, &source, &commit, &staged_vendor)?;
    write_ffmpeg_upstream_metadata(&staged_vendor, &source_url, &commit, source_date.trim())?;
    write_ffmpeg_vendor_metadata(&staged_vendor, &source_url, &commit, source_date.trim())?;

    let vendor_dir = ctx.path(ENHANCED_SYNC_FFMPEG_VENDOR_DIR);
    if vendor_dir.exists() {
        fs::remove_dir_all(&vendor_dir)
            .with_context(|| format!("remove {}", vendor_dir.display()))?;
    }
    if let Some(parent) = vendor_dir.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::rename(&staged_vendor, &vendor_dir).with_context(|| {
        format!(
            "move staged FFmpeg vendor tree into {}",
            vendor_dir.display()
        )
    })?;

    ok(format!(
        "vendored FFmpeg {} into {}",
        &commit[..12.min(commit.len())],
        ENHANCED_SYNC_FFMPEG_VENDOR_DIR
    ));
    Ok(())
}

fn run_vad_revendor(ctx: &TaskContext, args: VadRevendorArgs) -> Result<()> {
    step(format!(
        "Re-vendoring libfvad {} into enhanced subtitle sync",
        args.commit
    ));

    let scratch = tempfile::tempdir().context("create libfvad re-vendor scratch directory")?;
    let source = prepare_vad_source(ctx, &args, scratch.path())?;
    let commit = ensure_vad_commit(ctx, &source, &args.commit)?;
    let source_date = git_capture_in(
        ctx,
        &source,
        ["show", "-s", "--format=%cs", commit.as_str()],
    )
    .context("read libfvad commit date")?;
    let source_url = git_capture_in(ctx, &source, ["config", "--get", "remote.origin.url"])
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|_| args.source.clone());

    let staged_vendor = scratch.path().join("libfvad-vendor");
    fs::create_dir_all(&staged_vendor).context("create staged libfvad vendor directory")?;
    write_libfvad_vendor_archive(ctx, &source, &commit, &staged_vendor)?;
    write_libfvad_upstream_metadata(&staged_vendor, &source_url, &commit, source_date.trim())?;
    write_libfvad_vendor_metadata(&staged_vendor, &source_url, &commit, source_date.trim())?;

    let vendor_dir = ctx.path(ENHANCED_SYNC_LIBFVAD_VENDOR_DIR);
    if vendor_dir.exists() {
        fs::remove_dir_all(&vendor_dir)
            .with_context(|| format!("remove {}", vendor_dir.display()))?;
    }
    if let Some(parent) = vendor_dir.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::rename(&staged_vendor, &vendor_dir).with_context(|| {
        format!(
            "move staged libfvad vendor tree into {}",
            vendor_dir.display()
        )
    })?;

    ok(format!(
        "vendored libfvad {} into {}",
        &commit[..12.min(commit.len())],
        ENHANCED_SYNC_LIBFVAD_VENDOR_DIR
    ));
    Ok(())
}

fn prepare_ffmpeg_source(
    ctx: &TaskContext,
    args: &FfmpegRevendorArgs,
    scratch: &Path,
) -> Result<PathBuf> {
    let local_source = PathBuf::from(&args.source);
    if local_source.exists() {
        return Ok(local_source);
    }

    let clone_dir = scratch.join("ffmpeg-source");
    run_checked(
        ctx.command("git")
            .arg("clone")
            .arg("--filter=blob:none")
            .arg("--no-checkout")
            .arg(&args.source)
            .arg(&clone_dir),
    )
    .with_context(|| format!("clone FFmpeg source {}", args.source))?;
    Ok(clone_dir)
}

fn prepare_vad_source(
    ctx: &TaskContext,
    args: &VadRevendorArgs,
    scratch: &Path,
) -> Result<PathBuf> {
    let local_source = PathBuf::from(&args.source);
    if local_source.exists() {
        return Ok(local_source);
    }

    let clone_dir = scratch.join("libfvad-source");
    run_checked(
        ctx.command("git")
            .arg("clone")
            .arg("--filter=blob:none")
            .arg("--no-checkout")
            .arg(&args.source)
            .arg(&clone_dir),
    )
    .with_context(|| format!("clone libfvad source {}", args.source))?;
    Ok(clone_dir)
}

fn ensure_ffmpeg_commit(ctx: &TaskContext, source: &Path, commit: &str) -> Result<String> {
    if let Ok(resolved) =
        git_capture_in(ctx, source, ["rev-parse", &format!("{commit}^{{commit}}")])
    {
        return Ok(resolved.trim().to_string());
    }

    run_checked(
        ctx.command("git")
            .arg("-C")
            .arg(source)
            .arg("fetch")
            .arg("--depth=1")
            .arg("origin")
            .arg(commit),
    )
    .with_context(|| format!("fetch FFmpeg commit {commit}"))?;
    Ok(
        git_capture_in(ctx, source, ["rev-parse", &format!("{commit}^{{commit}}")])?
            .trim()
            .to_string(),
    )
}

fn ensure_vad_commit(ctx: &TaskContext, source: &Path, commit: &str) -> Result<String> {
    if let Ok(resolved) =
        git_capture_in(ctx, source, ["rev-parse", &format!("{commit}^{{commit}}")])
    {
        return Ok(resolved.trim().to_string());
    }

    run_checked(
        ctx.command("git")
            .arg("-C")
            .arg(source)
            .arg("fetch")
            .arg("--depth=1")
            .arg("origin")
            .arg(commit),
    )
    .with_context(|| format!("fetch libfvad commit {commit}"))?;
    Ok(
        git_capture_in(ctx, source, ["rev-parse", &format!("{commit}^{{commit}}")])?
            .trim()
            .to_string(),
    )
}

fn write_ffmpeg_vendor_archive(
    ctx: &TaskContext,
    source: &Path,
    commit: &str,
    destination: &Path,
) -> Result<()> {
    let archive_path = destination.join(ENHANCED_SYNC_FFMPEG_VENDOR_ARCHIVE);
    let mut archive = ctx
        .command("git")
        .arg("-C")
        .arg(source)
        .arg("archive")
        .arg(commit)
        .args(FFMPEG_VENDOR_PATHS)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("start git archive for FFmpeg commit {commit}"))?;
    let stdout = archive
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git archive stdout was not captured"))?;
    let file = fs::File::create(&archive_path)
        .with_context(|| format!("create {}", archive_path.display()))?;
    let writer = BufWriter::new(file);
    zstd::stream::copy_encode(stdout, writer, 19)
        .with_context(|| format!("compress FFmpeg vendor archive {}", archive_path.display()))?;
    let archive_status = archive.wait().context("wait for git archive")?;
    if !archive_status.success() {
        bail!("git archive failed with {archive_status}");
    }
    Ok(())
}

fn write_libfvad_vendor_archive(
    ctx: &TaskContext,
    source: &Path,
    commit: &str,
    destination: &Path,
) -> Result<()> {
    let archive_path = destination.join(ENHANCED_SYNC_LIBFVAD_VENDOR_ARCHIVE);
    let mut archive = ctx
        .command("git")
        .arg("-C")
        .arg(source)
        .arg("archive")
        .arg(commit)
        .args(LIBFVAD_VENDOR_PATHS)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("start git archive for libfvad commit {commit}"))?;
    let stdout = archive
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git archive stdout was not captured"))?;
    let file = fs::File::create(&archive_path)
        .with_context(|| format!("create {}", archive_path.display()))?;
    let writer = BufWriter::new(file);
    zstd::stream::copy_encode(stdout, writer, 19)
        .with_context(|| format!("compress libfvad vendor archive {}", archive_path.display()))?;
    let archive_status = archive.wait().context("wait for git archive")?;
    if !archive_status.success() {
        bail!("git archive failed with {archive_status}");
    }
    Ok(())
}

fn write_ffmpeg_upstream_metadata(
    vendor_dir: &Path,
    source_url: &str,
    commit: &str,
    source_date: &str,
) -> Result<()> {
    let metadata = format!(
        r#"# FFmpeg Source Snapshot

Vendored from FFmpeg upstream:

- repository: `{source_url}`
- commit: `{commit}`
- source date: `{source_date}`
- vendored for: targeted AC-3, E-AC-3, DTS/DCA, DTS-HD MA core fallback,
  and TrueHD/MLP decode-to-FLAC support

The plugin build configures this vendored tree as a narrow static FFmpeg
`avformat`/`avcodec`/`swresample`/`avutil` build and links it into the final
Rust `wasm32-wasip1` plugin artifact. FFmpeg source files are licensed by
FFmpeg under LGPL-2.1-or-later unless the individual file states otherwise.

Keep the configured build narrow: no programs, only the targeted audio
demuxers/muxer, no filters, and no network support.
"#
    );
    fs::write(vendor_dir.join("UPSTREAM.md"), metadata).context("write FFmpeg UPSTREAM.md")
}

fn write_libfvad_upstream_metadata(
    vendor_dir: &Path,
    source_url: &str,
    commit: &str,
    source_date: &str,
) -> Result<()> {
    fs::write(
        vendor_dir.join("UPSTREAM.md"),
        render_libfvad_upstream_metadata(source_url, commit, source_date),
    )
    .context("write libfvad UPSTREAM.md")
}

fn render_libfvad_upstream_metadata(source_url: &str, commit: &str, source_date: &str) -> String {
    format!(
        r#"# libfvad Source Snapshot

Vendored from libfvad upstream:

- repository: `{source_url}`
- commit: `{commit}`
- source date: `{source_date}`
- vendored for: WebRTC voice activity detection in the enhanced subtitle sync
  plugin

The plugin build compiles this vendored tree as a narrow static C library and
links it into the final Rust `wasm32-wasip1` plugin artifact. libfvad is a
standalone extraction of the WebRTC VAD engine; it is licensed under
BSD-3-Clause, with the additional patent grant included in `PATENTS`.

Keep the vendored archive narrow: only `include`, `src`, and attribution files
needed to rebuild and document the VAD backend belong in the archive.
"#
    )
}

fn write_ffmpeg_vendor_metadata(
    vendor_dir: &Path,
    source_url: &str,
    commit: &str,
    source_date: &str,
) -> Result<()> {
    let metadata = format!(
        "repository={source_url}\ncommit={commit}\nrevision=git-{commit}\nsource_date={source_date}\n"
    );
    fs::write(
        vendor_dir.join(ENHANCED_SYNC_FFMPEG_VENDOR_METADATA),
        metadata,
    )
    .context("write FFmpeg vendor metadata")
}

fn write_libfvad_vendor_metadata(
    vendor_dir: &Path,
    source_url: &str,
    commit: &str,
    source_date: &str,
) -> Result<()> {
    fs::write(
        vendor_dir.join(ENHANCED_SYNC_LIBFVAD_VENDOR_METADATA),
        render_vendor_metadata(source_url, commit, source_date),
    )
    .context("write libfvad vendor metadata")
}

fn render_vendor_metadata(source_url: &str, commit: &str, source_date: &str) -> String {
    format!(
        "repository={source_url}\ncommit={commit}\nrevision=git-{commit}\nsource_date={source_date}\n"
    )
}

fn git_capture_in<const N: usize>(
    ctx: &TaskContext,
    source: &Path,
    args: [&str; N],
) -> Result<String> {
    run_capture(ctx.command("git").arg("-C").arg(source).args(args))
}

fn run_release(ctx: &TaskContext, args: ReleaseArgs) -> Result<()> {
    let plugin = discover_local_official_plugin(ctx, &args.plugin_name)?;
    let target = resolve_release_target_for_plugin(ctx, &plugin, &args.options)?;
    run_release_targets(ctx, vec![target], &args.options)
}

fn run_release_many(ctx: &TaskContext, args: ReleaseManyArgs) -> Result<()> {
    if args.plugin_names.is_empty() {
        bail!("release-many requires at least one plugin id");
    }

    let mut targets = Vec::new();
    for plugin_name in &args.plugin_names {
        let plugin = discover_local_official_plugin(ctx, plugin_name)?;
        targets.push(resolve_release_target_for_plugin(
            ctx,
            &plugin,
            &args.options,
        )?);
    }
    run_release_targets(ctx, targets, &args.options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        NotificationCapabilities, NotificationDescriptor, current_sdk_constraint,
    };

    fn write_temp_manifest(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("create temp manifest");
        fs::write(file.path(), contents).expect("write temp manifest");
        file
    }

    fn local_plugin() -> LocalPluginInfo {
        LocalPluginInfo {
            plugin_id: "email".to_string(),
            name: "Email".to_string(),
            description: "Email notifications".to_string(),
            plugin_type: "notification".to_string(),
            provider_type: "email".to_string(),
            status: PluginCatalogStatus::Active,
            catalog_versions: default_catalog_versions(),
            feature_sets: default_feature_sets(),
            min_scryer_version: None,
            docs_url:
                "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
                    .to_string(),
            plugin_dir: PathBuf::from("/tmp/email"),
            cargo_toml: PathBuf::from("/tmp/email/Cargo.toml"),
            crate_name: "email".to_string(),
            current_version: Version::new(0, 1, 0),
            source_repo:
                "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
                    .to_string(),
            distribution_base_url: "https://cdn.scryer.media/scryer/plugins/email".to_string(),
        }
    }

    fn child_release(version: &str, sdk_constraint: &str) -> ChildCatalogReleaseV2 {
        ChildCatalogReleaseV2 {
            version: version.to_string(),
            sdk_constraint: sdk_constraint.to_string(),
            artifact_manifest_url: official_plugin_manifest_url("email", version),
        }
    }

    fn official_catalog_entry() -> CatalogV2Entry {
        CatalogV2Entry {
            id: "email".to_string(),
            name: "Email".to_string(),
            description: "Email notifications".to_string(),
            plugin_type: "notification".to_string(),
            provider_type: "email".to_string(),
            publisher: "scryer".to_string(),
            support_tier: "official".to_string(),
            docs_url:
                "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
                    .to_string(),
            source_repo:
                "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
                    .to_string(),
            child_catalog_url: official_plugin_child_catalog_url("email", "0.1.0"),
            required_signer: RequiredSignerV2 {
                github_repository: OFFICIAL_GITHUB_REPO.to_string(),
                github_workflow: Some(official_release_workflow()),
            },
        }
    }

    fn official_child_catalog(releases: Vec<ChildCatalogReleaseV2>) -> ChildCatalogV2 {
        let entry = official_catalog_entry();
        ChildCatalogV2 {
            schema_version: CHILD_CATALOG_V2_SCHEMA.to_string(),
            id: entry.id,
            name: entry.name,
            description: entry.description,
            plugin_type: entry.plugin_type,
            provider_type: entry.provider_type,
            publisher: entry.publisher,
            support_tier: entry.support_tier,
            docs_url: entry.docs_url,
            source_repo: entry.source_repo,
            releases,
        }
    }

    #[test]
    fn libfvad_vendor_metadata_matches_ffmpeg_style_keys() {
        let metadata = render_vendor_metadata(
            "https://github.com/dpirch/libfvad.git",
            "532ab666c20d3cfda38bca63abbb0f152706c369",
            "2024-01-02",
        );

        assert!(metadata.contains("repository=https://github.com/dpirch/libfvad.git\n"));
        assert!(metadata.contains("commit=532ab666c20d3cfda38bca63abbb0f152706c369\n"));
        assert!(metadata.contains("revision=git-532ab666c20d3cfda38bca63abbb0f152706c369\n"));
        assert!(metadata.contains("source_date=2024-01-02\n"));
    }

    #[test]
    fn libfvad_upstream_metadata_documents_vad_vendor_scope() {
        let metadata = render_libfvad_upstream_metadata(
            "https://github.com/dpirch/libfvad.git",
            "532ab666c20d3cfda38bca63abbb0f152706c369",
            "2024-01-02",
        );

        assert!(metadata.contains("# libfvad Source Snapshot"));
        assert!(metadata.contains("WebRTC voice activity detection"));
        assert!(metadata.contains("BSD-3-Clause"));
        assert!(metadata.contains("PATENTS"));
    }

    #[test]
    fn libfvad_vendor_archive_keeps_only_required_source_paths() {
        assert_eq!(
            LIBFVAD_VENDOR_PATHS,
            &[
                "AUTHORS",
                "LICENSE",
                "PATENTS",
                "README.md",
                "include",
                "src"
            ]
        );
    }

    #[test]
    fn validate_descriptor_contract_accepts_older_published_sdk_line() {
        let descriptor = PluginDescriptor {
            id: "qbittorrent".to_string(),
            name: "qBittorrent".to_string(),
            version: "0.1.8".to_string(),
            sdk_version: "1.5.0".to_string(),
            sdk_constraint: ">=1.5.0, <1.6.0".to_string(),
            socket_permissions: vec![],
            provider: ProviderDescriptor::Notification(NotificationDescriptor {
                provider_type: "qbittorrent-test".to_string(),
                provider_aliases: vec![],
                config_fields: vec![],
                default_base_url: None,
                allowed_hosts: vec![],
                capabilities: NotificationCapabilities::default(),
            }),
        };

        validate_descriptor_contract(&descriptor).expect("descriptor should validate");
        assert_eq!(
            plugin_descriptor_sdk_constraint(&descriptor),
            ">=1.5.0, <1.6.0"
        );
    }

    #[test]
    fn merge_catalog_plugin_entries_replaces_selected_entry_and_preserves_others() {
        let mut email = official_catalog_entry();
        email.child_catalog_url = official_plugin_child_catalog_url("email", "0.1.0");

        let mut qbittorrent = official_catalog_entry();
        qbittorrent.id = "qbittorrent".to_string();
        qbittorrent.name = "qBittorrent".to_string();
        qbittorrent.description = "Torrent download client".to_string();
        qbittorrent.provider_type = "qbittorrent".to_string();
        qbittorrent.docs_url =
            "https://github.com/scryer-media/scryer-plugins/tree/main/download_clients/qbittorrent"
                .to_string();
        qbittorrent.source_repo = qbittorrent.docs_url.clone();
        qbittorrent.child_catalog_url = official_plugin_child_catalog_url("qbittorrent", "0.1.6");

        let mut qbittorrent_updated = qbittorrent.clone();
        qbittorrent_updated.child_catalog_url =
            official_plugin_child_catalog_url("qbittorrent", "0.1.7");

        let merged = merge_catalog_plugin_entries(
            vec![qbittorrent, email.clone()],
            vec![qbittorrent_updated.clone()],
        );

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "email");
        assert_eq!(merged[0].child_catalog_url, email.child_catalog_url);
        assert_eq!(merged[1].id, "qbittorrent");
        assert_eq!(
            merged[1].child_catalog_url,
            qbittorrent_updated.child_catalog_url
        );
    }

    #[test]
    fn merge_catalog_v3_plugin_entries_replaces_selected_entry_and_preserves_others() {
        let email = CatalogV3PluginEntry {
            id: "email".to_string(),
            name: "Email".to_string(),
            description: "Email notifications".to_string(),
            plugin_type: "notification".to_string(),
            provider_type: "email".to_string(),
            publisher: "scryer".to_string(),
            support_tier: "official".to_string(),
            status: PluginCatalogStatus::Active,
            docs_url:
                "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
                    .to_string(),
            source_repo:
                "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
                    .to_string(),
            required_signer: official_required_signer(),
            releases: vec![CatalogV3Release {
                version: "0.1.0".to_string(),
                sdk_constraint: "^1.6.0".to_string(),
                min_scryer_version: None,
                artifacts: vec![CatalogV3PluginArtifact {
                    runtime: WASM_TARGET.to_string(),
                    required_features: Vec::new(),
                    wasm_digests: vec![
                        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_string(),
                        "shake256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    ],
                    bytes: 1234,
                    url: "https://cdn.scryer.media/scryer/plugins-v3/email/v0.1.0/plugin-v3.abc123.wasm.zst".to_string(),
                    mirror_urls: vec![
                        "https://github.com/scryer-media/scryer-plugins/releases/download/plugins-v3%2Femail%2Fv0.1.0/plugin-v3.abc123.wasm.zst".to_string(),
                    ],
                    signature_url: "https://cdn.scryer.media/scryer/plugins-v3/email/v0.1.0/plugin-v3.abc123.wasm.zst.bundle.zst".to_string(),
                    signature_mirror_urls: vec![
                        "https://github.com/scryer-media/scryer-plugins/releases/download/plugins-v3%2Femail%2Fv0.1.0/plugin-v3.abc123.wasm.zst.bundle.zst".to_string(),
                    ],
                    digests: vec![
                        "blake3:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                            .to_string(),
                        "shake256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                            .to_string(),
                    ],
                }],
            }],
        };
        let mut subdl = email.clone();
        subdl.id = "subdl".to_string();
        subdl.provider_type = "subdl".to_string();
        subdl.name = "Subdl".to_string();

        let mut subdl_updated = subdl.clone();
        subdl_updated.status = PluginCatalogStatus::Beta;

        let merged = merge_catalog_v3_plugin_entries(
            vec![subdl, email.clone()],
            vec![subdl_updated.clone()],
        );

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "email");
        assert_eq!(merged[1].id, "subdl");
        assert_eq!(merged[1].status, PluginCatalogStatus::Beta);
    }

    #[test]
    fn plugin_artifact_lane_file_names_do_not_overlap() {
        let baseline = WasmFeatureSet::baseline();
        let simd = WasmFeatureSet::new(vec![
            WasmRequiredFeature::Simd128,
            WasmRequiredFeature::RelaxedSimd,
        ]);

        assert_eq!(
            plugin_variant_logical_file_name(&baseline, PluginArtifactLane::V2, "zst"),
            "plugin.wasm.zst"
        );
        assert_eq!(
            plugin_variant_logical_file_name(&baseline, PluginArtifactLane::V3, "zst"),
            "plugin-v3.wasm.zst"
        );
        assert_eq!(
            plugin_variant_logical_file_name(&simd, PluginArtifactLane::V2, "br"),
            "plugin-simd128-relaxed-simd.wasm.br"
        );
        assert_eq!(
            plugin_variant_logical_file_name(&simd, PluginArtifactLane::V3, "br"),
            "plugin-v3-simd128-relaxed-simd.wasm.br"
        );
    }

    #[test]
    fn catalog_v3_validation_rejects_v2_release_lane_urls() {
        let mut entry = catalog_v3_plugin_entry(
            &local_plugin(),
            vec![CatalogV3Release {
                version: "0.1.0".to_string(),
                sdk_constraint: "^1.6.0".to_string(),
                min_scryer_version: None,
                artifacts: vec![CatalogV3PluginArtifact {
                    runtime: WASM_TARGET.to_string(),
                    required_features: Vec::new(),
                    wasm_digests: vec![
                        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_string(),
                    ],
                    bytes: 1234,
                    url: "https://cdn.scryer.media/scryer/plugins/email/v0.1.0/plugin.abc.wasm.zst"
                        .to_string(),
                    mirror_urls: vec![
                        "https://github.com/scryer-media/scryer-plugins/releases/download/plugins%2Femail%2Fv0.1.0/plugin.abc.wasm.zst".to_string(),
                    ],
                    signature_url: "https://cdn.scryer.media/scryer/plugins/email/v0.1.0/plugin.abc.wasm.zst.bundle.zst"
                        .to_string(),
                    signature_mirror_urls: Vec::new(),
                    digests: vec![
                        "blake3:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                            .to_string(),
                    ],
                }],
            }],
        );

        let error = validate_catalog_v3_plugin_entry(&entry)
            .expect_err("catalog-v3 should reject v2 lane URLs");
        assert!(error.to_string().contains("plugins-v3 lane"));

        entry.releases[0].artifacts[0].url =
            "https://cdn.scryer.media/scryer/plugins-v3/email/v0.1.0/plugin-v3.abc.wasm.zst"
                .to_string();
        entry.releases[0].artifacts[0].signature_url =
            "https://cdn.scryer.media/scryer/plugins-v3/email/v0.1.0/plugin-v3.abc.wasm.zst.bundle.zst"
                .to_string();
        entry.releases[0].artifacts[0].mirror_urls = vec![
            "https://github.com/scryer-media/scryer-plugins/releases/download/plugins-v3%2Femail%2Fv0.1.0/plugin-v3.abc.wasm.zst".to_string(),
        ];

        validate_catalog_v3_plugin_entry(&entry).expect("catalog-v3 should accept v3 lane URLs");
    }

    #[test]
    fn release_tag_version_accepts_new_and_legacy_tag_families() {
        assert_eq!(
            release_tag_version("email", "plugins/email/v1.2.3"),
            Some(Version::new(1, 2, 3))
        );
        assert_eq!(
            release_tag_version("email", "plugins-v3/email/v1.2.3"),
            Some(Version::new(1, 2, 3))
        );
        assert_eq!(
            release_tag_version("email", "email-v1.2.3"),
            Some(Version::new(1, 2, 3))
        );
        assert_eq!(release_tag_version("email", "plugins/other/v1.2.3"), None);
        assert_eq!(
            release_tag_version("email", "plugins/release/1746226197-f74cd0e"),
            None
        );
    }

    #[test]
    fn path_is_under_treats_exact_or_child_path_as_plugin_specific() {
        assert!(path_is_under("notifications/email", "notifications/email"));
        assert!(path_is_under(
            "notifications/email/src/lib.rs",
            "notifications/email"
        ));
        assert!(!path_is_under(
            "notifications/emailer/src/lib.rs",
            "notifications/email"
        ));
    }

    #[test]
    fn plugin_manifest_metadata_reads_official_fields_from_cargo_toml() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
status = "beta"
catalog_versions = ["v3"]
feature_sets = [{ required_features = [] }, { required_features = ["simd128", "relaxed-simd"] }]
docs_url = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
source_repo = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
distribution_base_url = "https://cdn.scryer.media/scryer/plugins/email"
min_scryer_version = "1.4.0"
"#,
        );

        let metadata = plugin_manifest_metadata(manifest.path()).expect("read manifest metadata");

        assert_eq!(metadata.description, "Email notifications");
        assert!(metadata.official);
        assert_eq!(metadata.plugin_id.as_deref(), Some("email"));
        assert_eq!(metadata.status, PluginCatalogStatus::Beta);
        assert_eq!(
            metadata.catalog_versions,
            BTreeSet::from([CatalogVersion::V3])
        );
        assert_eq!(
            metadata.feature_sets,
            vec![
                WasmFeatureSet::baseline(),
                WasmFeatureSet::new(vec![
                    WasmRequiredFeature::Simd128,
                    WasmRequiredFeature::RelaxedSimd,
                ]),
            ]
        );
        assert_eq!(
            metadata.docs_url.as_deref(),
            Some("https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email")
        );
        assert_eq!(
            metadata.source_repo.as_deref(),
            Some("https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email")
        );
        assert_eq!(
            metadata.distribution_base_url.as_deref(),
            Some("https://cdn.scryer.media/scryer/plugins/email")
        );
        assert_eq!(metadata.min_scryer_version.as_deref(), Some("1.4.0"));
    }

    #[test]
    fn plugin_manifest_metadata_requires_explicit_official_marker() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "whisper-subtitle-provider"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
"#,
        );

        let error = plugin_manifest_metadata(manifest.path()).expect_err("missing marker");

        assert!(
            error
                .to_string()
                .contains("package.metadata.scryer.official")
        );
    }

    #[test]
    fn validate_digest_string_accepts_blake3_and_shake256() {
        validate_digest_string(
            "digest",
            "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("blake3 should validate");
        validate_digest_string(
            "digest",
            "shake256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect("shake256 should validate");
    }

    #[test]
    fn validate_digest_string_rejects_unknown_algorithm() {
        let error =
            validate_digest_string("digest", "sha256:abcd").expect_err("digest should fail");

        assert!(error.to_string().contains("unsupported digest algorithm"));
    }

    #[test]
    fn plugin_manifest_metadata_requires_description_for_official_plugins() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"

[package.metadata.scryer]
official = true
plugin_id = "email"
docs_url = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
source_repo = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
distribution_base_url = "https://cdn.scryer.media/scryer/plugins/email"
"#,
        );

        let error =
            plugin_manifest_metadata(manifest.path()).expect_err("missing description should fail");

        assert!(error.to_string().contains("package.description"));
    }

    #[test]
    fn plugin_manifest_metadata_defaults_status_to_active() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
docs_url = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
source_repo = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
distribution_base_url = "https://cdn.scryer.media/scryer/plugins/email"
"#,
        );

        let metadata = plugin_manifest_metadata(manifest.path()).expect("status should default");

        assert_eq!(metadata.status, PluginCatalogStatus::Active);
        assert_eq!(metadata.catalog_versions, default_catalog_versions());
        assert_eq!(metadata.feature_sets, default_feature_sets());
        assert_eq!(metadata.min_scryer_version, None);
    }

    #[test]
    fn plugin_manifest_metadata_rejects_invalid_min_scryer_version() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
docs_url = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
source_repo = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
distribution_base_url = "https://cdn.scryer.media/scryer/plugins/email"
min_scryer_version = "soon"
"#,
        );

        let error =
            plugin_manifest_metadata(manifest.path()).expect_err("min_scryer_version should fail");

        assert!(error.to_string().contains("min_scryer_version"));
    }

    #[test]
    fn plugin_manifest_metadata_rejects_unknown_status() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
status = "preview"
"#,
        );

        let error = plugin_manifest_metadata(manifest.path()).expect_err("status should fail");

        assert!(
            error
                .to_string()
                .contains("unsupported package.metadata.scryer.status")
        );
    }

    #[test]
    fn plugin_manifest_metadata_rejects_unknown_catalog_version() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
catalog_versions = ["v4"]
"#,
        );

        let error =
            plugin_manifest_metadata(manifest.path()).expect_err("catalog_versions should fail");

        assert!(
            error
                .to_string()
                .contains("unsupported package.metadata.scryer.catalog_versions")
        );
    }

    #[test]
    fn plugin_manifest_metadata_rejects_unknown_wasm_required_feature() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
catalog_versions = ["v3"]
feature_sets = [{ required_features = ["threads"] }]
docs_url = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
source_repo = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
distribution_base_url = "https://cdn.scryer.media/scryer/plugins/email"
"#,
        );

        let error =
            plugin_manifest_metadata(manifest.path()).expect_err("wasm features should fail");

        assert!(
            error
                .to_string()
                .contains("package.metadata.scryer.feature_sets")
        );
    }

    #[test]
    fn plugin_manifest_metadata_requires_baseline_feature_set_for_v2() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
catalog_versions = ["v2", "v3"]
feature_sets = [{ required_features = ["simd128"] }]
docs_url = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
source_repo = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
distribution_base_url = "https://cdn.scryer.media/scryer/plugins/email"
"#,
        );

        let error = plugin_manifest_metadata(manifest.path())
            .expect_err("v2 publishing without baseline should fail");

        assert!(error.to_string().contains("required_features = []"));
    }

    #[test]
    fn plugin_manifest_metadata_rejects_relaxed_simd_without_simd128() {
        let manifest = write_temp_manifest(
            r#"[package]
name = "email-notification"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-only"
description = "Email notifications"

[package.metadata.scryer]
official = true
plugin_id = "email"
catalog_versions = ["v3"]
feature_sets = [{ required_features = ["relaxed-simd"] }]
docs_url = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
source_repo = "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
distribution_base_url = "https://cdn.scryer.media/scryer/plugins/email"
"#,
        );

        let error = plugin_manifest_metadata(manifest.path())
            .expect_err("relaxed-simd without simd128 should fail");

        assert!(error.to_string().contains("relaxed-simd requires simd128"));
    }

    #[test]
    fn child_catalog_preserves_historical_compatible_releases() {
        let catalog = child_catalog_from_local_plugin(
            &local_plugin(),
            vec![child_release("0.2.0", "^2"), child_release("0.1.0", "^1")],
        )
        .expect("child catalog");

        assert_eq!(
            catalog
                .releases
                .iter()
                .map(|release| release.version.as_str())
                .collect::<Vec<_>>(),
            vec!["0.1.0", "0.2.0"]
        );
    }

    #[test]
    fn child_catalog_rejects_same_version_with_different_manifest_url() {
        let mut duplicate = child_release("0.1.0", "^1");
        duplicate.artifact_manifest_url =
            "https://github.com/scryer-media/scryer-plugins/releases/download/plugins%2Femail%2Fv0.1.0/other.manifest.json".to_string();

        let error = child_catalog_from_local_plugin(
            &local_plugin(),
            vec![child_release("0.1.0", "^1"), duplicate],
        )
        .expect_err("duplicate manifest URL should fail");

        assert!(error.to_string().contains("multiple manifests"));
    }

    #[test]
    fn catalog_v2_supported_child_releases_drops_pre_15_history() {
        let releases = catalog_v2_supported_child_releases(vec![
            child_release("0.1.0", ">=1.3.0, <1.4.0"),
            child_release("0.2.0", ">=1.4.0, <1.5.0"),
            child_release("0.3.0", ">=1.5.0, <1.6.0"),
            child_release("0.4.0", ">=1.6.0, <1.7.0"),
        ])
        .expect("filtered child releases");

        assert_eq!(
            releases
                .iter()
                .map(|release| release.version.as_str())
                .collect::<Vec<_>>(),
            vec!["0.3.0", "0.4.0"]
        );
    }

    #[test]
    fn official_child_catalog_accepts_all_releases_on_current_sdk_line() {
        let entry = official_catalog_entry();
        let catalog = official_child_catalog(vec![
            child_release("0.1.0", ">=1.5.0, <1.6.0"),
            child_release("0.2.0", &current_sdk_constraint()),
        ]);

        validate_official_child_catalog(&catalog, &entry).expect("catalog should validate");
    }

    #[test]
    fn official_child_catalog_rejects_pre_15_release_history() {
        let entry = official_catalog_entry();
        let catalog = official_child_catalog(vec![
            child_release("0.1.0", ">=1.5.0, <1.6.0"),
            child_release("0.2.0", ">=1.4.0, <1.5.0"),
        ]);

        let error =
            validate_official_child_catalog(&catalog, &entry).expect_err("catalog should fail");

        assert!(
            error
                .to_string()
                .contains("predates the catalog-v2 base SDK")
        );
    }
}
