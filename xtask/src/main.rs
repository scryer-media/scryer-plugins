use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use extism::{Manifest, UserData, ValType, host_fn};
mod plugin_new;
use scryer_plugin_sdk::{
    EXPORT_DESCRIBE, EXPORT_DOWNLOAD_ADD, EXPORT_DOWNLOAD_CONTROL, EXPORT_DOWNLOAD_LIST_COMPLETED,
    EXPORT_DOWNLOAD_LIST_HISTORY, EXPORT_DOWNLOAD_LIST_QUEUE, EXPORT_DOWNLOAD_MARK_IMPORTED,
    EXPORT_DOWNLOAD_STATUS, EXPORT_DOWNLOAD_TEST_CONNECTION, EXPORT_INDEXER_SEARCH,
    EXPORT_NOTIFICATION_SEND, EXPORT_SUBTITLE_DOWNLOAD, EXPORT_SUBTITLE_GENERATE,
    EXPORT_SUBTITLE_SEARCH, EXPORT_VALIDATE_CONFIG, PluginDescriptor, ProviderDescriptor,
    SDK_VERSION, SubtitleProviderMode, host_version_matches_constraint,
    plugin_descriptor_sdk_constraint, validate_plugin_descriptor_host_permissions,
    validate_sdk_contract,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use toml_edit::{DocumentMut, value};

const BLUE: &str = "\x1b[0;34m";
const GREEN: &str = "\x1b[0;32m";
const YELLOW: &str = "\x1b[1;33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const TREE_REPO_PREFIX: &str = "https://github.com/scryer-media/scryer-plugins/tree/main/";
const WASM_TARGET: &str = "wasm32-wasip1";
const CATALOG_V2_SCHEMA: &str = "scryer.plugin.catalog.v2";
const CHILD_CATALOG_V2_SCHEMA: &str = "scryer.plugin.child_catalog.v2";
const PLUGIN_MANIFEST_SCHEMA: &str = "scryer.plugin.v1";
const WASM_OPT_LEVEL: &str = "-Oz";
const ZSTD_LEVEL: &str = "-19";
const OFFICIAL_GITHUB_REPO: &str = "scryer-media/scryer-plugins";
const OFFICIAL_RELEASE_WORKFLOW: &str = ".github/workflows/release-plugin.yml";
const CENTRAL_CATALOG_RELEASE_TAG: &str = "catalog/v2";
const CATALOG_V2_BASE_SDK_VERSION: &str = "1.5.0";
const RULE_PACK_SOURCE_MANIFEST: &str = "rule_packs/manifest.json";
const REPO_RELEASE_TAG_PREFIX: &str = "plugins/release/";
const CATALOG_PRETTY_JSON: &str = "catalog-v2.json";
const CATALOG_MINIFIED_JSON: &str = "catalog-v2.min.json";
const CATALOG_MINIFIED_ZST: &str = "catalog-v2.min.json.zst";
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
    VerifyPrepared(OfficialVerifyPreparedArgs),
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
}

#[derive(Args)]
struct OfficialPrefetchArgs {
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
struct CatalogArgs {
    #[command(subcommand)]
    command: CatalogCommand,
}

#[derive(Subcommand)]
enum CatalogCommand {
    RenderV2,
    PrepareV2(CatalogPrepareV2Args),
    PublishV2,
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
    plugin_dir: PathBuf,
    cargo_toml: PathBuf,
    crate_name: String,
    current_version: Version,
    source_repo: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PluginManifestMetadata {
    description: String,
    official: bool,
    plugin_id: Option<String>,
}

#[derive(Clone, Debug)]
struct CatalogAssetPaths {
    pretty_json: PathBuf,
    minified_json: PathBuf,
    minified_zst: PathBuf,
}

#[derive(Clone, Debug)]
struct OfficialPreparedRelease {
    dist: PathBuf,
    compressed_wasm: PathBuf,
    manifest_json: PathBuf,
    child_catalog: CatalogAssetPaths,
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
    asset: String,
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
        Commands::Sdk(args) => match args.command {
            SdkCommand::Bump { version } => run_sdk_bump(&ctx, &version),
        },
        Commands::Official(args) => match args.command {
            OfficialCommand::Release(args) => run_official_release(&ctx, args),
            OfficialCommand::Prepare(args) => run_official_prepare(&ctx, args),
            OfficialCommand::Prefetch(args) => run_official_prefetch(&ctx, args),
            OfficialCommand::PlanChanged(args) => run_official_plan_changed(&ctx, args),
            OfficialCommand::VerifyPrepared(args) => run_official_verify_prepared(&ctx, &args.dir),
        },
        Commands::Catalog(args) => match args.command {
            CatalogCommand::RenderV2 => run_catalog_render_v2(&ctx),
            CatalogCommand::PrepareV2(args) => run_catalog_prepare_v2(&ctx, args),
            CatalogCommand::PublishV2 => run_catalog_publish_v2(&ctx),
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
        .ok_or_else(|| anyhow!("{} must define [profile.plugin-release]", cargo_toml.display()))?;

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
    Ok(command)
}

fn repo_cargo_command_in(ctx: &TaskContext, cwd: &Path) -> Result<Command> {
    if let Some(rustup_toolchain) = configured_rustup_toolchain(ctx)? {
        return rustup_cargo_command_in(&rustup_toolchain, cwd);
    }

    Ok(ctx.command_in("cargo", cwd))
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
    let description = document
        .get("package")
        .and_then(|package| package.get("description"))
        .and_then(|description| description.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let official = document
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("scryer"))
        .and_then(|scryer| scryer.get("official"))
        .and_then(|official| official.as_bool())
        .ok_or_else(|| {
            anyhow!(
                "{} must define package.metadata.scryer.official as true or false",
                manifest_path.display()
            )
        })?;
    let plugin_id = document
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("scryer"))
        .and_then(|scryer| scryer.get("plugin_id"))
        .and_then(|plugin_id| plugin_id.as_str())
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
    }

    Ok(PluginManifestMetadata {
        description,
        official,
        plugin_id,
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

fn source_url_for_plugin_dir(ctx: &TaskContext, plugin_dir: &Path) -> Result<String> {
    let relative = plugin_dir.strip_prefix(&ctx.repo_root).with_context(|| {
        format!(
            "{} is not inside {}",
            plugin_dir.display(),
            ctx.repo_root.display()
        )
    })?;
    Ok(format!("{TREE_REPO_PREFIX}{}", relative.display()))
}

fn discover_local_plugin(ctx: &TaskContext, plugin_dir: &Path) -> Result<LocalPluginInfo> {
    let cargo_toml = plugin_dir.join("Cargo.toml");
    let crate_name = crate_name_from_manifest(&cargo_toml)?;
    let current_version = version_from_manifest(&cargo_toml)?;
    let manifest_metadata = plugin_manifest_metadata(&cargo_toml)?;
    let description = manifest_metadata.description.clone();
    let source_repo = source_url_for_plugin_dir(ctx, plugin_dir)?;
    let wasm = build_plugin_wasm(ctx, plugin_dir)?;
    let descriptor = load_descriptor_from_wasm(&wasm)?;
    validate_descriptor_contract(&descriptor)?;
    let manifest_plugin_id = manifest_metadata.plugin_id.as_deref().ok_or_else(|| {
        anyhow!(
            "{} is missing package.metadata.scryer.plugin_id",
            cargo_toml.display()
        )
    })?;
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
        plugin_dir: plugin_dir.to_path_buf(),
        cargo_toml,
        crate_name,
        current_version,
        source_repo,
    })
}

fn discover_local_plugins(ctx: &TaskContext) -> Result<Vec<LocalPluginInfo>> {
    local_plugin_directories(ctx)?
        .into_iter()
        .map(|plugin_dir| discover_local_plugin(ctx, &plugin_dir))
        .collect()
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
    let has_existing_release = !existing_releases.is_empty();
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

fn release_tag_version(plugin_id: &str, tag: &str) -> Option<Version> {
    tag.strip_prefix(&release_tag_prefix(plugin_id))
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
        let built_wasm = build_plugin_wasm(ctx, &target.plugin_dir)?;
        ok("Built release WASM");

        step(format!("Validating {}", target.plugin_id));
        let descriptor = load_descriptor_from_wasm(&built_wasm)?;
        validate_descriptor_contract(&descriptor)?;
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

fn build_plugin_wasm(ctx: &TaskContext, plugin_dir: &Path) -> Result<PathBuf> {
    let cargo_toml = plugin_dir.join("Cargo.toml");
    validate_plugin_release_profile(&cargo_toml)?;
    let wasm_filename = wasm_filename_for_manifest(&cargo_toml)?;

    step(format!("Building {}", plugin_dir.display()));
    ensure_lockfile(ctx, plugin_dir)?;
    let mut build = wasm_build_command_in(ctx, plugin_dir)?;
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

    let built_wasm = cargo_target_dir(plugin_dir)
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

fn load_descriptor_from_wasm(wasm_path: &Path) -> Result<PluginDescriptor> {
    let bytes =
        fs::read(wasm_path).with_context(|| format!("failed to read {}", wasm_path.display()))?;
    let manifest =
        Manifest::new([extism::Wasm::data(bytes)]).with_timeout(std::time::Duration::from_secs(10));
    let socket_stubs = UserData::new(());
    let mut plugin = extism::PluginBuilder::new(manifest)
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
        .with_context(|| format!("failed to instantiate {}", wasm_path.display()))?;

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

    let wasm_path = build_plugin_wasm(ctx, &plugin_dir)?;
    let descriptor = load_descriptor_from_wasm(&wasm_path)?;
    validate_descriptor_contract(&descriptor)?;
    ok(format!(
        "Validated {} {} ({})",
        descriptor.id,
        descriptor.version,
        descriptor.plugin_type()
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
        "release artifacts use wasm-opt {WASM_OPT_LEVEL} and zstd {ZSTD_LEVEL}"
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
    local_plugin_directories(ctx)
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
        let manifest_path = plugin_dir.join("Cargo.toml");
        let mut metadata = repo_cargo_command_in(ctx, plugin_dir)?;
        metadata
            .arg("metadata")
            .args(["--format-version", "1", "--manifest-path"])
            .arg(&manifest_path)
            .arg("--locked");
        let raw = run_capture(&mut metadata)?;
        let value: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse cargo metadata for {}", plugin_id))?;
        let packages = value
            .get("packages")
            .and_then(|packages| packages.as_array())
            .ok_or_else(|| anyhow!("cargo metadata for {} did not contain packages", plugin_id))?;

        for package in packages {
            let Some(package_manifest) = package.get("manifest_path").and_then(|v| v.as_str())
            else {
                continue;
            };
            let package_manifest = PathBuf::from(package_manifest);
            if package_manifest.starts_with(&ctx.repo_root)
                && let Some(package_dir) = package_manifest.parent()
            {
                dirs.insert(package_dir.to_path_buf());
            }
        }
    }

    dirs.insert(ctx.repo_root.join("xtask"));
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
        build_plugin_wasm(ctx, &dir)?;
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

fn optimize_and_compress_wasm(
    ctx: &TaskContext,
    wasm: &Path,
    dist: &Path,
) -> Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(dist)?;
    let optimized = dist.join("plugin.wasm");
    let compressed = dist.join("plugin.wasm.zst");
    run_checked(
        ctx.command("wasm-opt")
            .arg(WASM_OPT_LEVEL)
            .arg("--enable-bulk-memory-opt")
            .arg(wasm)
            .arg("-o")
            .arg(&optimized),
    )?;
    run_checked(
        ctx.command("zstd")
            .arg(ZSTD_LEVEL)
            .arg("-f")
            .arg(&optimized)
            .arg("-o")
            .arg(&compressed),
    )?;
    Ok((optimized, compressed))
}

fn github_release_asset_url(repo: &str, tag: &str, asset: &str) -> String {
    let tag = tag.replace('/', "%2F");
    format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
}

fn official_plugin_release_tag(plugin_id: &str, version: &str) -> String {
    format!("plugins/{plugin_id}/v{version}")
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
        docs_url: plugin.source_repo.clone(),
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

fn catalog_asset_paths(dir: &Path) -> CatalogAssetPaths {
    CatalogAssetPaths {
        pretty_json: dir.join(CATALOG_PRETTY_JSON),
        minified_json: dir.join(CATALOG_MINIFIED_JSON),
        minified_zst: dir.join(CATALOG_MINIFIED_ZST),
    }
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

fn read_catalog_bytes(ctx: &TaskContext, path: &Path) -> Result<Vec<u8>> {
    if path.extension().and_then(OsStr::to_str) == Some("zst") {
        return Ok(run_capture(ctx.command("zstd").arg("-dc").arg(path))?.into_bytes());
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
    let existing_releases = resolve_existing_child_catalog_releases(
        ctx,
        &plugin.plugin_id,
        args.existing_child_catalog.as_deref(),
    )?;
    let wasm = build_plugin_wasm(ctx, &plugin.plugin_dir)?;
    let dist = args
        .out
        .unwrap_or_else(|| default_child_catalog_dir(ctx, &plugin.plugin_id));
    let (optimized, compressed) = optimize_and_compress_wasm(ctx, &wasm, &dist)?;
    let descriptor = load_descriptor_from_wasm(&optimized)?;
    validate_descriptor_contract(&descriptor)?;
    let version = args.version.unwrap_or_else(|| descriptor.version.clone());
    let manifest = PluginManifestV2 {
        schema_version: PLUGIN_MANIFEST_SCHEMA.to_string(),
        id: descriptor.id.clone(),
        plugin_type: descriptor.plugin_type().to_string(),
        provider_type: descriptor.provider_type().to_string(),
        version,
        publisher: "scryer".to_string(),
        artifact: "plugin.wasm.zst".to_string(),
        compression: "zstd".to_string(),
        wasm_digest: blake3_file(&optimized)?,
        artifact_digest: blake3_file(&compressed)?,
        signature: "plugin.wasm.zst.bundle".to_string(),
    };
    let manifest_path = dist.join("plugin.manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    let child_catalog = write_child_catalog_to_dir(
        ctx,
        &plugin,
        Some(ChildCatalogReleaseV2 {
            version: manifest.version.clone(),
            sdk_constraint: plugin_descriptor_sdk_constraint(&descriptor),
            artifact_manifest_url: official_plugin_manifest_url(
                &plugin.plugin_id,
                &manifest.version,
            ),
        }),
        existing_releases,
        &dist,
    )?;
    Ok(OfficialPreparedRelease {
        dist,
        compressed_wasm: compressed,
        manifest_json: manifest_path,
        child_catalog,
    })
}

fn run_official_prepare(ctx: &TaskContext, args: OfficialPrepareArgs) -> Result<()> {
    let prepared = prepare_official_release(ctx, args)?;
    ok(format!(
        "wrote unsigned release assets to {}",
        prepared.dist.display()
    ));
    println!("   Artifact : {}", prepared.compressed_wasm.display());
    println!("   Manifest : {}", prepared.manifest_json.display());
    println!(
        "   Catalog  : {}",
        prepared.child_catalog.pretty_json.display()
    );
    println!(
        "   Runtime  : {}",
        prepared.child_catalog.minified_zst.display()
    );
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
    let compressed_wasm = dir.join("plugin.wasm.zst");
    let wasm = dir.join("plugin.wasm");
    let manifest_path = dir.join("plugin.manifest.json");
    let catalog_paths = catalog_asset_paths(dir);
    for path in [
        &compressed_wasm,
        &wasm,
        &manifest_path,
        &catalog_paths.pretty_json,
        &catalog_paths.minified_json,
        &catalog_paths.minified_zst,
    ] {
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
        serde_json::from_slice(&fs::read(&catalog_paths.pretty_json)?)
            .with_context(|| format!("failed to parse {}", catalog_paths.pretty_json.display()))?;
    let runtime_value: serde_json::Value =
        serde_json::from_slice(&read_catalog_bytes(ctx, &catalog_paths.minified_zst)?)?;
    if pretty_value != runtime_value {
        bail!("pretty child catalog and minified zstd child catalog decode to different JSON");
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
        docs_url: plugin.source_repo.clone(),
        source_repo: plugin.source_repo.clone(),
        child_catalog_url: official_plugin_child_catalog_url(&plugin.plugin_id, &version),
        required_signer: RequiredSignerV2 {
            github_repository: OFFICIAL_GITHUB_REPO.to_string(),
            github_workflow: Some(OFFICIAL_RELEASE_WORKFLOW.to_string()),
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
            github_workflow: Some(OFFICIAL_RELEASE_WORKFLOW.to_string()),
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
        let source_path = ctx.repo_root.join("rule_packs").join(&asset_name);
        let manifest = load_rule_pack_manifest(&source_path)?;
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

fn run_catalog_prepare_v2(ctx: &TaskContext, args: CatalogPrepareV2Args) -> Result<()> {
    let plugins = if args.plugin_ids.is_empty() {
        step("Preparing catalog-v2 assets from local official plugin descriptors");
        discover_local_plugins(ctx)?
            .iter()
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
                let plugin = discover_local_official_plugin(ctx, plugin_id)?;
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
        output_dir.join(".github/workflows/release-plugin.yml"),
        "name: release-plugin\non:\n  push:\n    tags: ['v*']\npermissions:\n  contents: write\n  id-token: write\njobs:\n  build-sign-release:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - uses: sigstore/cosign-installer@v4.1.1\n        with:\n          cosign-release: v3.0.2\n      - run: echo 'Adapt this workflow to build wasm32-wasip1, wasm-opt -Oz, zstd -19, and cosign sign-blob.'\n",
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

fn cosign_verify_blob_with_identity_pattern(
    ctx: &TaskContext,
    blob: &Path,
    bundle: &Path,
    identity_pattern: &str,
) -> Result<()> {
    run_checked(
        ctx.command("cosign")
            .arg("verify-blob")
            .arg("--bundle")
            .arg(bundle)
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
        regex_escape_literal(OFFICIAL_RELEASE_WORKFLOW),
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
        if plugin.required_signer.github_workflow.as_deref() != Some(OFFICIAL_RELEASE_WORKFLOW) {
            bail!(
                "{}: required_signer.github_workflow must be {}",
                plugin.id,
                OFFICIAL_RELEASE_WORKFLOW
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
    use scryer_plugin_sdk::{NotificationCapabilities, NotificationDescriptor};

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
            plugin_dir: PathBuf::from("/tmp/email"),
            cargo_toml: PathBuf::from("/tmp/email/Cargo.toml"),
            crate_name: "email".to_string(),
            current_version: Version::new(0, 1, 0),
            source_repo:
                "https://github.com/scryer-media/scryer-plugins/tree/main/notifications/email"
                    .to_string(),
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
                github_workflow: Some(OFFICIAL_RELEASE_WORKFLOW.to_string()),
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
    fn release_tag_version_accepts_new_and_legacy_tag_families() {
        assert_eq!(
            release_tag_version("email", "plugins/email/v1.2.3"),
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
"#,
        );

        let metadata = plugin_manifest_metadata(manifest.path()).expect("read manifest metadata");

        assert_eq!(metadata.description, "Email notifications");
        assert!(metadata.official);
        assert_eq!(metadata.plugin_id.as_deref(), Some("email"));
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
"#,
        );

        let error =
            plugin_manifest_metadata(manifest.path()).expect_err("missing description should fail");

        assert!(error.to_string().contains("package.description"));
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
            child_release("0.2.0", ">=1.6.0, <1.7.0"),
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
