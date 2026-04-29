use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use extism::Manifest;
use scryer_plugin_sdk::{
    EXPORT_DESCRIBE, EXPORT_DOWNLOAD_ADD, EXPORT_DOWNLOAD_CONTROL, EXPORT_DOWNLOAD_LIST_COMPLETED,
    EXPORT_DOWNLOAD_LIST_HISTORY, EXPORT_DOWNLOAD_LIST_QUEUE, EXPORT_DOWNLOAD_MARK_IMPORTED,
    EXPORT_DOWNLOAD_STATUS, EXPORT_DOWNLOAD_TEST_CONNECTION, EXPORT_INDEXER_SEARCH,
    EXPORT_NOTIFICATION_SEND, EXPORT_SUBTITLE_DOWNLOAD, EXPORT_SUBTITLE_GENERATE,
    EXPORT_SUBTITLE_SEARCH, EXPORT_VALIDATE_CONFIG, PluginDescriptor, ProviderDescriptor,
    SDK_VERSION, SubtitleProviderMode,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use toml_edit::{DocumentMut, value};

const BLUE: &str = "\x1b[0;34m";
const GREEN: &str = "\x1b[0;32m";
const YELLOW: &str = "\x1b[1;33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const RAW_PREFIX: &str = "https://raw.githubusercontent.com/scryer-media/scryer-plugins/main/dist/";
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

#[derive(Debug, Deserialize, Serialize)]
struct Registry {
    plugins: Vec<RegistryPlugin>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RegistryPlugin {
    id: String,
    name: String,
    plugin_type: String,
    provider_type: String,
    version: String,
    sdk_version: String,
    #[serde(default)]
    builtin: bool,
    #[serde(default)]
    wasm_url: Option<String>,
    #[serde(default)]
    wasm_sha256: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
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
    Ok(serde_json::from_str(&content)?)
}

fn save_registry(ctx: &TaskContext, registry: &Registry) -> Result<()> {
    fs::write(
        registry_path(ctx),
        serde_json::to_string_pretty(registry)? + "\n",
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

fn validate_registry(ctx: &TaskContext) -> Result<()> {
    let registry = load_registry(ctx)?;
    let dist_dir = ctx.path("dist");
    let mut errors = Vec::new();

    for plugin in &registry.plugins {
        let artifact_path = if plugin.builtin {
            match plugin_source_dir(ctx, plugin).and_then(|plugin_dir| build_plugin_wasm(ctx, &plugin_dir))
            {
                Ok(path) => path,
                Err(error) => {
                    errors.push(format!("{}: {error}", plugin.id));
                    continue;
                }
            }
        } else {
            let Some(wasm_url) = plugin.wasm_url.as_deref() else {
                errors.push(format!("{}: missing wasm_url", plugin.id));
                continue;
            };
            let Some(wasm_sha256) = plugin.wasm_sha256.as_deref() else {
                errors.push(format!("{}: missing wasm_sha256", plugin.id));
                continue;
            };
            if !wasm_url.starts_with(RAW_PREFIX) {
                errors.push(format!(
                    "{}: wasm_url must start with {}",
                    plugin.id, RAW_PREFIX
                ));
                continue;
            }

            let artifact_name = wasm_url.trim_start_matches(RAW_PREFIX);
            let artifact_path = dist_dir.join(artifact_name);
            if !artifact_path.is_file() {
                errors.push(format!(
                    "{}: missing dist artifact {}",
                    plugin.id, artifact_name
                ));
                continue;
            }

            let actual_sha = sha256_file(&artifact_path)?;
            if actual_sha != wasm_sha256 {
                errors.push(format!(
                    "{}: sha256 mismatch (registry={}, actual={})",
                    plugin.id, wasm_sha256, actual_sha
                ));
            }

            artifact_path
        };

        match load_descriptor_from_wasm(&artifact_path).and_then(|descriptor| {
            validate_descriptor_contract(&descriptor)?;
            validate_registry_entry_matches_descriptor(plugin, &descriptor)
        }) {
            Ok(()) => {}
            Err(error) => errors.push(format!("{}: {error}", plugin.id)),
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

fn plugin_source_dir(ctx: &TaskContext, plugin: &RegistryPlugin) -> Result<PathBuf> {
    let source_url = plugin
        .source_url
        .as_deref()
        .ok_or_else(|| anyhow!("{}: builtin plugin is missing source_url", plugin.id))?;
    let relative = source_url
        .split("/tree/main/")
        .nth(1)
        .ok_or_else(|| anyhow!("{}: unsupported source_url {}", plugin.id, source_url))?;
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
        build.args(["build", "--release", "--target", WASM_TARGET, "--offline"]);
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
    build.args(["build", "--release", "--target", WASM_TARGET, "--offline"]);
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
    let sdk_major = descriptor.sdk_version.split('.').next().unwrap_or_default();
    let supported_major = SDK_VERSION.split('.').next().unwrap_or_default();
    if sdk_major != supported_major {
        bail!(
            "{}: unsupported sdk_version {} (expected major {})",
            descriptor.id,
            descriptor.sdk_version,
            supported_major
        );
    }
    if descriptor.id.trim().is_empty() {
        bail!("descriptor id must not be empty");
    }
    if descriptor.provider_type().trim().is_empty() {
        bail!("{}: provider_type must not be empty", descriptor.id);
    }
    for host in descriptor.allowed_hosts() {
        if !allowed_host_pattern_is_valid(host) {
            bail!("{}: invalid network permission pattern {}", descriptor.id, host);
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
        return !suffix.is_empty()
            && !suffix.contains('*')
            && url::Host::parse(suffix).is_ok();
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

    validate_registry_entry_matches_descriptor(entry, descriptor)
}

fn validate_registry_entry_matches_descriptor(
    entry: &RegistryPlugin,
    descriptor: &PluginDescriptor,
) -> Result<()> {
    let expected = [
        ("id", entry.id.as_str(), descriptor.id.as_str()),
        (
            "version",
            entry.version.as_str(),
            descriptor.version.as_str(),
        ),
        (
            "sdk_version",
            entry.sdk_version.as_str(),
            descriptor.sdk_version.as_str(),
        ),
        (
            "plugin_type",
            entry.plugin_type.as_str(),
            descriptor.plugin_type(),
        ),
        (
            "provider_type",
            entry.provider_type.as_str(),
            descriptor.provider_type(),
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
        provider: {provider_variant},
    }};
    Ok(serde_json::to_string(&descriptor)?)
}}
{family_exports}
"#
    )
}

fn run_release(ctx: &TaskContext, args: ReleaseArgs) -> Result<()> {
    let plugin_dir = locate_plugin(ctx, &args.plugin_name)?;
    let cargo_toml = plugin_dir.join("Cargo.toml");
    let crate_name = crate_name_from_manifest(&cargo_toml)?;
    let current_version = version_from_manifest(&cargo_toml)?;
    let (bump, explicit) = parse_bump(&args)?;
    let next_version = explicit.unwrap_or_else(|| next_version(&current_version, bump));
    let tag_name = format!("{}-v{}", args.plugin_name, next_version);
    let wasm_filename = crate_name.replace('-', "_") + ".wasm";

    step("Determining next version");
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

    let mut registry = load_registry(ctx)?;
    let plugin = registry
        .plugins
        .iter_mut()
        .find(|plugin| plugin.id == args.plugin_name)
        .ok_or_else(|| anyhow!("Plugin '{}' not found in registry.json", args.plugin_name))?;
    if plugin.builtin {
        bail!(
            "Plugin '{}' is builtin — builtin plugins are released with scryer, not independently",
            args.plugin_name
        );
    }

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
    plugin.version = next_version.to_string();
    plugin.wasm_url = Some(format!("{RAW_PREFIX}{wasm_filename}"));
    plugin.wasm_sha256 = Some(sha256.clone());
    save_registry(ctx, &registry)?;
    ok(format!(
        "registry.json updated (version={}, sha256={sha256})",
        next_version
    ));

    step("Validating registry");
    validate_registry(ctx)?;
    ok("Registry validation passed");

    if args.dry_run {
        println!("\n{YELLOW}{BOLD}Dry run complete — stopping before commit/tag/push.{RESET}");
        println!("  {} {} validated OK.", args.plugin_name, next_version);
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
        &format!("release: {} {}", args.plugin_name, next_version),
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
