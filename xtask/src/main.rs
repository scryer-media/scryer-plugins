use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use extism::Manifest;
use scryer_plugin_sdk::{
    plugin_descriptor_sdk_constraint, validate_sdk_contract, PluginDescriptor, ProviderDescriptor,
    SubtitleProviderMode, EXPORT_DESCRIBE, EXPORT_DOWNLOAD_ADD, EXPORT_DOWNLOAD_CONTROL,
    EXPORT_DOWNLOAD_LIST_COMPLETED, EXPORT_DOWNLOAD_LIST_HISTORY, EXPORT_DOWNLOAD_LIST_QUEUE,
    EXPORT_DOWNLOAD_MARK_IMPORTED, EXPORT_DOWNLOAD_STATUS, EXPORT_DOWNLOAD_TEST_CONNECTION,
    EXPORT_INDEXER_SEARCH, EXPORT_NOTIFICATION_SEND, EXPORT_SUBTITLE_DOWNLOAD,
    EXPORT_SUBTITLE_GENERATE, EXPORT_SUBTITLE_SEARCH, EXPORT_VALIDATE_CONFIG, SDK_VERSION,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use toml_edit::{value, DocumentMut};

const BLUE: &str = "\x1b[0;34m";
const GREEN: &str = "\x1b[0;32m";
const YELLOW: &str = "\x1b[1;33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const RAW_REPO_PREFIX: &str = "https://raw.githubusercontent.com/scryer-media/scryer-plugins/";
const WASM_TARGET: &str = "wasm32-wasip1";

struct BuiltinPluginSpec {
    plugin_dir: &'static str,
    artifact_name: &'static str,
}

const BUILTIN_PLUGINS: &[BuiltinPluginSpec] = &[
    BuiltinPluginSpec {
        plugin_dir: "indexers/nzbgeek",
        artifact_name: "nzbgeek_indexer.wasm",
    },
    BuiltinPluginSpec {
        plugin_dir: "indexers/newznab",
        artifact_name: "newznab_indexer.wasm",
    },
    BuiltinPluginSpec {
        plugin_dir: "indexers/dognzb",
        artifact_name: "dognzb_indexer.wasm",
    },
    BuiltinPluginSpec {
        plugin_dir: "indexers/animetosho",
        artifact_name: "animetosho_indexer.wasm",
    },
    BuiltinPluginSpec {
        plugin_dir: "indexers/torznab",
        artifact_name: "torznab_indexer.wasm",
    },
    BuiltinPluginSpec {
        plugin_dir: "subtitles/jimaku",
        artifact_name: "jimaku_subtitle_provider.wasm",
    },
];

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
    Release(ReleaseArgs),
    Registry(RegistryArgs),
    Builtins(BuiltinsArgs),
    Plugin(PluginArgs),
}

#[derive(Args)]
struct ReleaseArgs {
    plugin_name: String,
    #[arg(long, conflicts_with_all = ["minor", "patch", "version"])]
    major: bool,
    #[arg(long, conflicts_with_all = ["major", "patch", "version"])]
    minor: bool,
    #[arg(long, conflicts_with_all = ["major", "minor", "version"])]
    patch: bool,
    #[arg(long)]
    dry_run: bool,
    version: Option<String>,
}

#[derive(Args)]
struct RegistryArgs {
    #[command(subcommand)]
    command: RegistryCommand,
}

#[derive(Args)]
struct BuiltinsArgs {
    #[arg(long, value_name = "DIR")]
    output_dir: Option<PathBuf>,
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

#[derive(Copy, Clone, Eq, PartialEq, ValueEnum)]
enum PluginKindArg {
    Indexer,
    DownloadClient,
    Notification,
    Subtitle,
}

#[derive(Subcommand)]
enum RegistryCommand {
    Validate,
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
struct Registry {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    plugins: Vec<RegistryPlugin>,
    #[serde(default)]
    rule_packs: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RegistryPlugin {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    plugin_type: String,
    provider_type: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    official: bool,
    #[serde(default)]
    releases: Vec<RegistryRelease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sdk_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sdk_constraint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    builtin: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wasm_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wasm_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scryer_constraint: Option<String>,
    #[serde(
        default,
        rename = "min_scryer_version",
        skip_serializing_if = "Option::is_none"
    )]
    legacy_min_scryer_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RegistryRelease {
    version: String,
    sdk_version: String,
    #[serde(default)]
    sdk_constraint: String,
    #[serde(default)]
    builtin: bool,
    #[serde(default)]
    wasm_url: Option<String>,
    #[serde(default)]
    wasm_sha256: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    scryer_constraint: Option<String>,
    #[serde(default, rename = "min_scryer_version", skip_serializing_if = "Option::is_none")]
    legacy_min_scryer_version: Option<String>,
}

fn default_schema_version() -> u32 {
    1
}

impl RegistryPlugin {
    fn normalized_releases(&self) -> Vec<RegistryRelease> {
        if !self.releases.is_empty() {
            return self.releases.clone();
        }

        self.version
            .as_ref()
            .map(|version| {
                vec![RegistryRelease {
                    version: version.clone(),
                    sdk_version: self.sdk_version.clone().unwrap_or_default(),
                    sdk_constraint: self.sdk_constraint.clone().unwrap_or_default(),
                    builtin: self.builtin.unwrap_or(false),
                    wasm_url: self.wasm_url.clone(),
                    wasm_sha256: self.wasm_sha256.clone(),
                    source_url: self.source_url.clone(),
                    scryer_constraint: self.scryer_constraint.clone(),
                    legacy_min_scryer_version: self.legacy_min_scryer_version.clone(),
                }]
            })
            .unwrap_or_default()
    }

    fn canonicalize(&mut self) {
        if self.releases.is_empty() {
            self.releases = self.normalized_releases();
        }
        self.releases.sort_by(|left, right| {
            parse_release_version(&self.id, left)
                .ok()
                .cmp(&parse_release_version(&self.id, right).ok())
        });
        self.version = None;
        self.sdk_version = None;
        self.sdk_constraint = None;
        self.builtin = None;
        self.wasm_url = None;
        self.wasm_sha256 = None;
        self.source_url = None;
        self.scryer_constraint = None;
        self.legacy_min_scryer_version = None;
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let ctx = TaskContext::new();

    match cli.command {
        Commands::Release(args) => run_release(&ctx, args),
        Commands::Registry(args) => match args.command {
            RegistryCommand::Validate => validate_registry(&ctx),
        },
        Commands::Builtins(args) => run_builtins(&ctx, args),
        Commands::Plugin(args) => match args.command {
            PluginCommand::New(args) => run_plugin_new(&ctx, args),
            PluginCommand::Validate(args) => run_plugin_validate(&ctx, args),
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

fn command_available(command: &str) -> Result<bool> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {command} >/dev/null 2>&1"))
        .status()?;
    Ok(status.success())
}

fn require_command(command: &str) -> Result<()> {
    if command_available(command)? {
        Ok(())
    } else {
        bail!("{command} is required")
    }
}

fn require_wasm_target(ctx: &TaskContext) -> Result<()> {
    require_command("rustup")?;
    let mut targets = ctx.command("rustup");
    targets.args(["target", "list", "--installed"]);
    let installed_targets = run_capture(&mut targets)?;
    if !installed_targets.lines().any(|line| line == WASM_TARGET) {
        bail!("{WASM_TARGET} target not installed — run: rustup target add {WASM_TARGET}");
    }
    Ok(())
}

fn git_capture(ctx: &TaskContext, args: &[&str]) -> Result<String> {
    let mut command = ctx.command_in("git", &ctx.repo_root);
    command.args(args);
    run_capture(&mut command)
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

fn parse_bump(args: &ReleaseArgs) -> Result<(VersionBump, Option<Version>)> {
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

fn registry_path(ctx: &TaskContext) -> PathBuf {
    ctx.path("registry.json")
}

fn load_registry(ctx: &TaskContext) -> Result<Registry> {
    let content = fs::read_to_string(registry_path(ctx))?;
    let mut registry: Registry = serde_json::from_str(&content)?;
    for plugin in &mut registry.plugins {
        plugin.canonicalize();
    }
    Ok(registry)
}

fn save_registry(ctx: &TaskContext, registry: &Registry) -> Result<()> {
    let mut registry = Registry {
        schema_version: registry.schema_version,
        plugins: registry.plugins.clone(),
        rule_packs: registry.rule_packs.clone(),
    };
    for plugin in &mut registry.plugins {
        plugin.canonicalize();
    }
    fs::write(
        registry_path(ctx),
        serde_json::to_string_pretty(&registry)? + "\n",
    )?;
    Ok(())
}

fn locate_plugin(ctx: &TaskContext, plugin_name: &str) -> Result<PathBuf> {
    for prefix in ["indexers", "download_clients", "notifications", "subtitles"] {
        let path = ctx.path(prefix).join(plugin_name);
        if path.is_dir() {
            return Ok(path);
        }
    }
    bail!("Plugin '{plugin_name}' not found in any plugin directory")
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
    command.args(paths);
    run_checked(&mut command)
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(bytes);
    Ok(format!("{digest:x}"))
}

fn release_wasm_url(tag_name: &str, wasm_filename: &str) -> String {
    format!("{RAW_REPO_PREFIX}{tag_name}/dist/{wasm_filename}")
}

fn artifact_name_from_wasm_url(wasm_url: &str) -> Result<&str> {
    let suffix = wasm_url
        .strip_prefix(RAW_REPO_PREFIX)
        .ok_or_else(|| anyhow!("wasm_url must start with {RAW_REPO_PREFIX}"))?;
    let (git_ref, artifact_name) = suffix
        .split_once("/dist/")
        .ok_or_else(|| anyhow!("wasm_url must contain /dist/ after the git ref"))?;
    if git_ref.trim().is_empty() {
        bail!("wasm_url is missing a git ref");
    }
    if git_ref == "main" {
        bail!("wasm_url must use an immutable tag or commit ref, not main");
    }
    if artifact_name.trim().is_empty() {
        bail!("wasm_url is missing an artifact name");
    }
    Ok(artifact_name)
}

fn parse_release_version(plugin_id: &str, release: &RegistryRelease) -> Result<Version> {
    Version::parse(release.version.trim())
        .with_context(|| format!("{plugin_id}: invalid release version {}", release.version))
}

fn latest_release(plugin: &RegistryPlugin) -> Result<RegistryRelease> {
    plugin
        .normalized_releases()
        .into_iter()
        .max_by(|left, right| {
            parse_release_version(&plugin.id, left)
                .ok()
                .cmp(&parse_release_version(&plugin.id, right).ok())
        })
        .ok_or_else(|| anyhow!("{}: registry entry has no releases", plugin.id))
}

fn latest_builtin_release(plugin: &RegistryPlugin) -> Result<Option<RegistryRelease>> {
    Ok(plugin
        .normalized_releases()
        .into_iter()
        .filter(|release| release.builtin)
        .max_by(|left, right| {
            parse_release_version(&plugin.id, left)
                .ok()
                .cmp(&parse_release_version(&plugin.id, right).ok())
        }))
}

fn registry_release_sdk_constraint(release: &RegistryRelease) -> String {
    scryer_plugin_sdk::sdk_constraint_or_legacy(&release.sdk_version, &release.sdk_constraint)
}

fn registry_release_scryer_constraint(release: &RegistryRelease) -> Option<String> {
    release
        .scryer_constraint
        .as_deref()
        .map(str::trim)
        .filter(|constraint| !constraint.is_empty())
        .map(str::to_string)
        .or_else(|| {
            release
                .legacy_min_scryer_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .map(|version| format!(">={version}"))
        })
}

fn validate_registry_release_scryer_constraint(
    plugin_id: &str,
    release: &RegistryRelease,
) -> Result<()> {
    let Some(constraint) = registry_release_scryer_constraint(release) else {
        return Ok(());
    };
    semver::VersionReq::parse(constraint.trim()).map_err(|error| {
        anyhow!(
            "{} {}: invalid scryer_constraint {}: {error}",
            plugin_id,
            release.version,
            constraint
        )
    })?;
    Ok(())
}

fn validate_registry(ctx: &TaskContext) -> Result<()> {
    let registry = load_registry(ctx)?;
    let dist_dir = ctx.path("dist");
    let mut errors = Vec::new();

    for plugin in &registry.plugins {
        let latest_builtin = match latest_builtin_release(plugin) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("{}: {error}", plugin.id));
                continue;
            }
        };

        for release in plugin.normalized_releases() {
            if let Err(error) = validate_registry_release_scryer_constraint(&plugin.id, &release) {
                errors.push(error.to_string());
                continue;
            }
            let artifact_path = if let Some(wasm_url) = release.wasm_url.as_deref() {
                let Some(wasm_sha256) = release.wasm_sha256.as_deref() else {
                    errors.push(format!(
                        "{} {}: missing wasm_sha256",
                        plugin.id, release.version
                    ));
                    continue;
                };
                let artifact_name = match artifact_name_from_wasm_url(wasm_url) {
                    Ok(value) => value,
                    Err(error) => {
                        errors.push(format!("{} {}: {error}", plugin.id, release.version));
                        continue;
                    }
                };
                let artifact_path = dist_dir.join(artifact_name);
                if !artifact_path.is_file() {
                    errors.push(format!(
                        "{} {}: missing dist artifact {}",
                        plugin.id, release.version, artifact_name
                    ));
                    continue;
                }

                let actual_sha = sha256_file(&artifact_path)?;
                if actual_sha != wasm_sha256 {
                    errors.push(format!(
                        "{} {}: sha256 mismatch (registry={}, actual={})",
                        plugin.id, release.version, wasm_sha256, actual_sha
                    ));
                }

                artifact_path
            } else if latest_builtin
                .as_ref()
                .is_some_and(|builtin| builtin.version == release.version)
            {
                let Some(source_url) = release.source_url.as_deref() else {
                    errors.push(format!(
                        "{} {}: builtin release is missing source_url",
                        plugin.id, release.version
                    ));
                    continue;
                };
                match plugin_source_dir(ctx, &plugin.id, source_url)
                    .and_then(|plugin_dir| build_plugin_wasm(ctx, &plugin_dir))
                {
                    Ok(path) => path,
                    Err(error) => {
                        errors.push(format!("{} {}: {error}", plugin.id, release.version));
                        continue;
                    }
                }
            } else if release.builtin {
                continue;
            } else {
                errors.push(format!(
                    "{} {}: missing wasm_url for downloadable release",
                    plugin.id, release.version
                ));
                continue;
            };

            match load_descriptor_from_wasm(&artifact_path).and_then(|descriptor| {
                validate_descriptor_contract(&descriptor)?;
                validate_registry_entry_matches_descriptor(plugin, &release, &descriptor)
            }) {
                Ok(()) => {}
                Err(error) => errors.push(format!("{} {}: {error}", plugin.id, release.version)),
            }
        }
    }

    if errors.is_empty() {
        println!("registry OK");
        Ok(())
    } else {
        for error in errors {
            eprintln!("{error}");
        }
        bail!("registry validation failed");
    }
}

fn plugin_source_dir(ctx: &TaskContext, plugin_id: &str, source_url: &str) -> Result<PathBuf> {
    let relative = source_url
        .split("/tree/main/")
        .nth(1)
        .ok_or_else(|| anyhow!("{plugin_id}: unsupported source_url {source_url}"))?;
    Ok(ctx.path(relative))
}

fn run_builtins(ctx: &TaskContext, args: BuiltinsArgs) -> Result<()> {
    require_wasm_target(ctx)?;

    let output_dir = args.output_dir.unwrap_or_else(|| ctx.path("dist/builtins"));
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;

    step(format!(
        "Building {} built-in plugin WASM artifact(s)",
        BUILTIN_PLUGINS.len()
    ));
    for spec in BUILTIN_PLUGINS {
        let plugin_dir = ctx.path(spec.plugin_dir);
        if !plugin_dir.is_dir() {
            bail!(
                "built-in plugin directory missing: {}",
                plugin_dir.display()
            );
        }

        ensure_lockfile(ctx, &plugin_dir)?;
        let mut build = ctx.command_in("cargo", &plugin_dir);
        build.args([
            "build",
            "--release",
            "--target",
            WASM_TARGET,
            "--locked",
            "--offline",
        ]);
        run_checked(&mut build).with_context(|| format!("failed to build {}", spec.plugin_dir))?;

        let built_wasm = plugin_dir
            .join("target")
            .join(WASM_TARGET)
            .join("release")
            .join(spec.artifact_name);
        if !built_wasm.is_file() {
            bail!("expected WASM at {} but not found", built_wasm.display());
        }

        let output_wasm = output_dir.join(spec.artifact_name);
        fs::copy(&built_wasm, &output_wasm).with_context(|| {
            format!(
                "failed to copy {} to {}",
                built_wasm.display(),
                output_wasm.display()
            )
        })?;
        let sha256 = sha256_file(&output_wasm)?;
        println!(
            "   {} -> {} ({sha256})",
            spec.plugin_dir,
            output_wasm.display()
        );
    }

    ok(format!(
        "Copied built-in plugin artifacts to {}",
        output_dir.display()
    ));
    Ok(())
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
    let mut command = ctx.command_in("cargo", plugin_dir);
    command.args(["generate-lockfile", "--offline"]);
    run_checked(&mut command)
        .with_context(|| format!("failed to generate lockfile for {}", plugin_dir.display()))
}

fn build_plugin_wasm(ctx: &TaskContext, plugin_dir: &Path) -> Result<PathBuf> {
    require_wasm_target(ctx)?;
    let cargo_toml = plugin_dir.join("Cargo.toml");
    let wasm_filename = wasm_filename_for_manifest(&cargo_toml)?;

    step(format!("Building {}", plugin_dir.display()));
    ensure_lockfile(ctx, plugin_dir)?;
    let mut build = ctx.command_in("cargo", plugin_dir);
    build.args([
        "build",
        "--release",
        "--target",
        WASM_TARGET,
        "--locked",
        "--offline",
    ]);
    run_checked(&mut build)?;

    let built_wasm = plugin_dir
        .join("target")
        .join(WASM_TARGET)
        .join("release")
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
    let mut plugin = extism::PluginBuilder::new(manifest)
        .with_wasi(true)
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
        SDK_VERSION,
    )?;
    if descriptor.id.trim().is_empty() {
        bail!("descriptor id must not be empty");
    }
    if descriptor.provider_type().trim().is_empty() {
        bail!("{}: provider_type must not be empty", descriptor.id);
    }
    for host in descriptor.allowed_hosts() {
        if !allowed_host_pattern_is_valid(host) {
            bail!(
                "{}: invalid network permission pattern {}",
                descriptor.id,
                host
            );
        }
    }
    Ok(())
}

fn allowed_host_pattern_is_valid(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty()
        || host == "*"
        || host.contains("://")
        || host.contains('/')
        || host.contains('?')
        || host.contains('#')
        || host.contains(':')
    {
        return false;
    }

    if let Some(suffix) = host.strip_prefix("*.") {
        return !suffix.is_empty() && !suffix.contains('*') && url::Host::parse(suffix).is_ok();
    }

    !host.contains('*') && url::Host::parse(host).is_ok()
}

fn validate_descriptor_against_registry(
    ctx: &TaskContext,
    descriptor: &PluginDescriptor,
) -> Result<()> {
    let registry = load_registry(ctx)?;
    let Some(entry) = registry
        .plugins
        .iter()
        .find(|plugin| plugin.id == descriptor.id)
    else {
        warn(format!(
            "{} is not present in registry.json; skipping registry comparison",
            descriptor.id
        ));
        return Ok(());
    };

    let release = latest_release(entry)?;
    validate_registry_entry_matches_descriptor(entry, &release, descriptor)
}

fn validate_registry_entry_matches_descriptor(
    entry: &RegistryPlugin,
    release: &RegistryRelease,
    descriptor: &PluginDescriptor,
) -> Result<()> {
    let expected_sdk_constraint = registry_release_sdk_constraint(release);
    let descriptor_sdk_constraint = plugin_descriptor_sdk_constraint(descriptor);
    let expected = vec![
        ("id", entry.id.clone(), descriptor.id.clone()),
        (
            "version",
            release.version.clone(),
            descriptor.version.clone(),
        ),
        (
            "sdk_version",
            release.sdk_version.clone(),
            descriptor.sdk_version.clone(),
        ),
        (
            "sdk_constraint",
            expected_sdk_constraint,
            descriptor_sdk_constraint,
        ),
        (
            "plugin_type",
            entry.plugin_type.clone(),
            descriptor.plugin_type().to_string(),
        ),
        (
            "provider_type",
            entry.provider_type.clone(),
            descriptor.provider_type().to_string(),
        ),
    ];
    for (field, registry_value, descriptor_value) in expected {
        if registry_value != descriptor_value {
            bail!(
                "{}: registry {field}={} does not match descriptor {field}={}",
                descriptor.id,
                registry_value,
                descriptor_value
            );
        }
    }
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
    validate_descriptor_against_registry(ctx, &descriptor)?;
    ok(format!(
        "Validated {} {} ({})",
        descriptor.id,
        descriptor.version,
        descriptor.plugin_type()
    ));
    Ok(())
}

fn plugin_kind_directory(kind: PluginKindArg) -> &'static str {
    match kind {
        PluginKindArg::Indexer => "indexers",
        PluginKindArg::DownloadClient => "download_clients",
        PluginKindArg::Notification => "notifications",
        PluginKindArg::Subtitle => "subtitles",
    }
}

fn plugin_kind_crate_suffix(kind: PluginKindArg) -> &'static str {
    match kind {
        PluginKindArg::Indexer => "indexer",
        PluginKindArg::DownloadClient => "download_client",
        PluginKindArg::Notification => "notification",
        PluginKindArg::Subtitle => "subtitle_provider",
    }
}

fn normalize_plugin_name(name: &str) -> Result<String> {
    let normalized = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if normalized.is_empty() {
        bail!("plugin name must contain at least one ASCII letter or digit");
    }
    Ok(normalized)
}

fn run_plugin_new(ctx: &TaskContext, args: PluginNewArgs) -> Result<()> {
    let plugin_id = normalize_plugin_name(&args.name)?;
    let plugin_dir = ctx
        .repo_root
        .join(plugin_kind_directory(args.kind))
        .join(&plugin_id);
    if plugin_dir.exists() {
        bail!("{} already exists", plugin_dir.display());
    }
    fs::create_dir_all(plugin_dir.join("src"))?;

    let crate_name = format!(
        "{}_{}",
        plugin_id.replace('-', "_"),
        plugin_kind_crate_suffix(args.kind)
    );
    let cargo_toml = format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
extism-pdk = "1"
scryer-plugin-sdk = {{ path = "../../../scryer/crates/scryer-plugin-sdk" }}
serde_json = "1"
"#
    );
    fs::write(plugin_dir.join("Cargo.toml"), cargo_toml)?;

    let lib_rs = plugin_scaffold_source(args.kind, &plugin_id);
    fs::write(plugin_dir.join("src/lib.rs"), lib_rs)?;
    ok(format!("Created {}", plugin_dir.display()));
    Ok(())
}

fn plugin_scaffold_source(kind: PluginKindArg, plugin_id: &str) -> String {
    let provider_variant = match kind {
        PluginKindArg::Indexer => format!(
            r#"ProviderDescriptor::Indexer(IndexerDescriptor {{
            provider_type: "{plugin_id}".to_string(),
            provider_aliases: vec![],
            source_kind: IndexerSourceKind::Generic,
            capabilities: IndexerCapabilities::default(),
            scoring_policies: vec![],
            config_fields: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }})"#
        ),
        PluginKindArg::DownloadClient => format!(
            r#"ProviderDescriptor::DownloadClient(DownloadClientDescriptor {{
            provider_type: "{plugin_id}".to_string(),
            provider_aliases: vec![],
            config_fields: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![],
            isolation_modes: vec![],
            capabilities: DownloadClientCapabilities::default(),
        }})"#
        ),
        PluginKindArg::Notification => format!(
            r#"ProviderDescriptor::Notification(NotificationDescriptor {{
            provider_type: "{plugin_id}".to_string(),
            provider_aliases: vec![],
            config_fields: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities::default(),
        }})"#
        ),
        PluginKindArg::Subtitle => format!(
            r#"ProviderDescriptor::Subtitle(SubtitleDescriptor {{
            provider_type: "{plugin_id}".to_string(),
            provider_aliases: vec![],
            config_fields: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: SubtitleCapabilities {{
                mode: SubtitleProviderMode::Catalog,
                ..SubtitleCapabilities::default()
            }},
        }})"#
        ),
    };

    let family_exports = match kind {
        PluginKindArg::Indexer => {
            r#"
#[plugin_fn]
pub fn scryer_indexer_search(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(PluginSearchResponse::default()))?)
}
"#
        }
        PluginKindArg::DownloadClient => {
            r#"
#[plugin_fn]
pub fn scryer_download_add(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::<PluginDownloadClientAddResponse>::Err(PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: "download add is not implemented".to_string(),
        debug_message: None,
        retry_after_seconds: None,
    }))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue() -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(Vec::<PluginDownloadItem>::new()))?)
}

#[plugin_fn]
pub fn scryer_download_list_history() -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(Vec::<PluginCompletedDownload>::new()))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed() -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(Vec::<PluginCompletedDownload>::new()))?)
}

#[plugin_fn]
pub fn scryer_download_control(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status() -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(PluginDownloadClientStatus::default()))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection() -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}
"#
        }
        PluginKindArg::Notification => {
            r#"
#[plugin_fn]
pub fn scryer_notification_send(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(PluginNotificationResponse {
        success: true,
        error: None,
    }))?)
}
"#
        }
        PluginKindArg::Subtitle => {
            r#"
#[plugin_fn]
pub fn scryer_validate_config(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(SubtitlePluginValidateConfigResponse {
        status: SubtitleValidateConfigStatus::Valid,
        message: None,
        retry_after_seconds: None,
    }))?)
}

#[plugin_fn]
pub fn scryer_subtitle_search(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(SubtitlePluginSearchResponse::default()))?)
}

#[plugin_fn]
pub fn scryer_subtitle_download(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::<SubtitlePluginDownloadResponse>::Err(PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: "subtitle download is not implemented".to_string(),
        debug_message: None,
        retry_after_seconds: None,
    }))?)
}
"#
        }
    };

    format!(
        r#"use extism_pdk::*;
use scryer_plugin_sdk::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {{
    let descriptor = PluginDescriptor {{
        id: "{plugin_id}".to_string(),
        name: "{plugin_id}".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        provider: {provider_variant},
    }};
    Ok(serde_json::to_string(&descriptor)?)
}}
{family_exports}
"#
    )
}

fn run_release(ctx: &TaskContext, args: ReleaseArgs) -> Result<()> {
    let mut registry = load_registry(ctx)?;
    let plugin = registry
        .plugins
        .iter_mut()
        .find(|plugin| plugin.id == args.plugin_name)
        .ok_or_else(|| anyhow!("Plugin '{}' not found in registry.json", args.plugin_name))?;
    let plugin_id = plugin.id.clone();
    let latest = latest_release(plugin)?;
    let source_url = latest.source_url.clone().ok_or_else(|| {
        anyhow!(
            "Plugin '{}' is missing source_url in its latest release",
            plugin_id
        )
    })?;
    let plugin_dir = plugin_source_dir(ctx, &plugin_id, &source_url)?;
    let cargo_toml = plugin_dir.join("Cargo.toml");
    let crate_name = crate_name_from_manifest(&cargo_toml)?;
    let current_version = version_from_manifest(&cargo_toml)?;
    let (bump, explicit) = parse_bump(&args)?;
    let next_version = explicit.unwrap_or_else(|| next_version(&current_version, bump));
    let next_version_text = next_version.to_string();
    if plugin
        .normalized_releases()
        .iter()
        .any(|release| release.version == next_version_text)
    {
        bail!(
            "Plugin '{}' already has a {} release in registry.json",
            plugin_id,
            next_version
        );
    }
    let tag_name = format!("{}-v{}", plugin_id, next_version);
    let wasm_filename = crate_name.replace('-', "_") + ".wasm";

    step("Determining next version");
    println!("   Plugin ID  : {}", plugin_id);
    println!("   Plugin dir : {}", plugin_dir.display());
    println!("   Crate name : {crate_name}");
    println!("   WASM file  : {wasm_filename}");
    println!("   Current    : {current_version}");
    println!("   Next       : {next_version}");
    println!("   Tag        : {tag_name}");
    if args.dry_run {
        println!("   {YELLOW}(dry run — no commits, tags, or pushes){RESET}");
    }

    step("Pre-flight checks");
    let tags = git_capture(ctx, &["tag"])?;
    if tags.lines().any(|line| line == tag_name) {
        bail!("Tag {tag_name} already exists");
    }
    let branch = current_branch(ctx)?;
    println!("   Branch: {branch}");
    prompt_continue_if_dirty(ctx)?;

    require_wasm_target(ctx)?;
    ok("Pre-flight OK");

    step(format!("Bumping {crate_name} to {next_version}"));
    write_manifest_version(&cargo_toml, &next_version)?;
    ok("Cargo.toml updated");

    step("Building WASM (release, wasm32-wasip1)");
    let mut build = ctx.command_in("cargo", &plugin_dir);
    build.args([
        "build",
        "--release",
        "--target",
        "wasm32-wasip1",
        "--locked",
    ]);
    run_checked(&mut build)?;
    let built_wasm = plugin_dir
        .join("target/wasm32-wasip1/release")
        .join(&wasm_filename);
    if !built_wasm.is_file() {
        bail!("Expected WASM at {} but not found", built_wasm.display());
    }
    ok(format!("Built {wasm_filename}"));

    step("Validating built descriptor");
    let descriptor = load_descriptor_from_wasm(&built_wasm)?;
    validate_descriptor_contract(&descriptor)?;
    let descriptor_version = Version::parse(&descriptor.version).with_context(|| {
        format!(
            "{}: descriptor version {} is not valid semver",
            descriptor.id, descriptor.version
        )
    })?;
    if descriptor.id != plugin_id {
        bail!(
            "built descriptor id {} does not match registry plugin id {}",
            descriptor.id,
            plugin_id
        );
    }
    if descriptor_version != next_version {
        bail!(
            "{}: built descriptor version {} does not match requested release version {}",
            descriptor.id,
            descriptor.version,
            next_version
        );
    }
    ok(format!(
        "Validated descriptor {} {} ({})",
        descriptor.id,
        descriptor.version,
        descriptor.plugin_type()
    ));

    step(format!("Updating dist/{wasm_filename}"));
    let dist_dir = ctx.path("dist");
    fs::create_dir_all(&dist_dir)?;
    let dist_wasm = dist_dir.join(&wasm_filename);
    let existed_before = dist_wasm.exists();
    fs::copy(&built_wasm, &dist_wasm)?;
    let sha256 = sha256_file(&dist_wasm)?;
    println!("   SHA256: {sha256}");
    ok("Copied to dist/");

    step("Updating registry.json");
    let release = RegistryRelease {
        version: descriptor.version.clone(),
        sdk_version: descriptor.sdk_version.clone(),
        sdk_constraint: plugin_descriptor_sdk_constraint(&descriptor),
        builtin: false,
        wasm_url: Some(release_wasm_url(&tag_name, &wasm_filename)),
        wasm_sha256: Some(sha256.clone()),
        source_url: Some(source_url),
        scryer_constraint: registry_release_scryer_constraint(&latest),
        legacy_min_scryer_version: None,
    };
    validate_registry_entry_matches_descriptor(plugin, &release, &descriptor)?;
    plugin.releases.push(release);
    plugin.canonicalize();
    save_registry(ctx, &registry)?;
    ok(format!(
        "registry.json updated (appended version={}, sha256={sha256})",
        descriptor.version
    ));

    step("Validating registry");
    validate_registry(ctx)?;
    ok("Registry validation passed");

    if args.dry_run {
        println!("\n{YELLOW}{BOLD}Dry run complete — stopping before commit/tag/push.{RESET}");
        println!("  {} {} validated OK.", plugin_id, next_version);
        let restore = vec![cargo_toml.clone(), registry_path(ctx)];
        git_checkout_paths(ctx, &restore)?;
        if existed_before {
            let _ = git_checkout_paths(ctx, &[dist_wasm.clone()]);
        } else {
            let _ = fs::remove_file(dist_wasm);
        }
        return Ok(());
    }

    step("Committing changes");
    let mut add = ctx.command_in("git", &ctx.repo_root);
    add.arg("add")
        .arg(&cargo_toml)
        .arg(registry_path(ctx))
        .arg(&dist_wasm);
    run_checked(&mut add)?;
    let mut commit = ctx.command_in("git", &ctx.repo_root);
    commit.args([
        "commit",
        "-m",
        &format!("release: {} {}", plugin_id, next_version),
    ]);
    run_checked(&mut commit)?;
    ok("Committed");

    step(format!("Creating signed tag {tag_name}"));
    let mut tag = ctx.command_in("git", &ctx.repo_root);
    tag.args(["tag", "-s", &tag_name, "-m", &format!("Release {tag_name}")]);
    run_checked(&mut tag)?;
    ok(format!("Tag {tag_name} created"));

    step("Pushing to origin");
    let mut push_branch = ctx.command_in("git", &ctx.repo_root);
    push_branch.args(["push", "origin", &branch]);
    run_checked(&mut push_branch)?;
    let mut push_tag = ctx.command_in("git", &ctx.repo_root);
    push_tag.args(["push", "origin", &tag_name]);
    run_checked(&mut push_tag)?;
    ok(format!("Pushed {branch} and tag {tag_name}"));

    println!("\n{GREEN}{BOLD}Released {tag_name}{RESET}");
    Ok(())
}
