use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
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
    version: String,
    #[serde(default)]
    builtin: bool,
    #[serde(default)]
    wasm_url: Option<String>,
    #[serde(default)]
    wasm_sha256: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let ctx = TaskContext::new();

    match cli.command {
        Commands::Release(args) => run_release(&ctx, args),
        Commands::Registry(args) => match args.command {
            RegistryCommand::Validate => validate_registry(&ctx),
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
        if plugin.builtin {
            continue;
        }
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

    require_command("rustup")?;
    let mut targets = ctx.command("rustup");
    targets.args(["target", "list", "--installed"]);
    let installed_targets = run_capture(&mut targets)?;
    if !installed_targets
        .lines()
        .any(|line| line == "wasm32-wasip1")
    {
        bail!("wasm32-wasip1 target not installed — run: rustup target add wasm32-wasip1");
    }
    ok("Pre-flight OK");

    step(format!("Bumping {crate_name} to {next_version}"));
    write_manifest_version(&cargo_toml, &next_version)?;
    ok("Cargo.toml updated");

    step("Building WASM (release, wasm32-wasip1)");
    let mut build = ctx.command_in("cargo", &plugin_dir);
    build.args(["build", "--release", "--target", "wasm32-wasip1"]);
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
