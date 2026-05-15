use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, Item, value};

use crate::{
    PluginKindArg, PluginNewArgs, SDK_VERSION, TaskContext, ok, repo_cargo_command_in, run_checked,
};

const LIB_RS_TEMPLATE: &str = include_str!("../templates/plugin_new/lib.rs.tpl");
const INDEXER_IMPORTS_TEMPLATE: &str = include_str!("../templates/plugin_new/imports/indexer.txt");
const DOWNLOAD_CLIENT_IMPORTS_TEMPLATE: &str =
    include_str!("../templates/plugin_new/imports/download_client.txt");
const NOTIFICATION_IMPORTS_TEMPLATE: &str =
    include_str!("../templates/plugin_new/imports/notification.txt");
const SUBTITLE_IMPORTS_TEMPLATE: &str =
    include_str!("../templates/plugin_new/imports/subtitle.txt");
const INDEXER_PROVIDER_TEMPLATE: &str =
    include_str!("../templates/plugin_new/provider/indexer.rs.tpl");
const DOWNLOAD_CLIENT_PROVIDER_TEMPLATE: &str =
    include_str!("../templates/plugin_new/provider/download_client.rs.tpl");
const NOTIFICATION_PROVIDER_TEMPLATE: &str =
    include_str!("../templates/plugin_new/provider/notification.rs.tpl");
const SUBTITLE_PROVIDER_TEMPLATE: &str =
    include_str!("../templates/plugin_new/provider/subtitle.rs.tpl");
const INDEXER_EXPORTS_TEMPLATE: &str =
    include_str!("../templates/plugin_new/exports/indexer.rs.tpl");
const DOWNLOAD_CLIENT_EXPORTS_TEMPLATE: &str =
    include_str!("../templates/plugin_new/exports/download_client.rs.tpl");
const NOTIFICATION_EXPORTS_TEMPLATE: &str =
    include_str!("../templates/plugin_new/exports/notification.rs.tpl");
const SUBTITLE_EXPORTS_TEMPLATE: &str =
    include_str!("../templates/plugin_new/exports/subtitle.rs.tpl");

pub(crate) fn run_plugin_new(ctx: &TaskContext, args: PluginNewArgs) -> Result<()> {
    let plugin_dir = scaffold_plugin(ctx, &ctx.repo_root, args.kind, &args.name)?;
    ok(format!("Created {}", plugin_dir.display()));
    Ok(())
}

fn scaffold_plugin(
    ctx: &TaskContext,
    repo_root: &Path,
    kind: PluginKindArg,
    name: &str,
) -> Result<PathBuf> {
    let spec = ScaffoldSpec::new(kind, name)?;
    let plugin_dir = repo_root.join(spec.kind.directory()).join(&spec.plugin_id);
    if plugin_dir.exists() {
        bail!("{} already exists", plugin_dir.display());
    }

    fs::create_dir_all(plugin_dir.join("src"))?;
    let scaffold_result = (|| -> Result<()> {
        fs::write(plugin_dir.join("Cargo.toml"), spec.render_manifest())?;
        fs::write(plugin_dir.join("src/lib.rs"), spec.render_lib_rs()?)?;
        format_generated_crate(ctx, &plugin_dir)?;
        Ok(())
    })();

    if let Err(error) = scaffold_result {
        let _ = fs::remove_dir_all(&plugin_dir);
        return Err(error);
    }

    Ok(plugin_dir)
}

fn format_generated_crate(ctx: &TaskContext, plugin_dir: &Path) -> Result<()> {
    let mut fmt = repo_cargo_command_in(ctx, plugin_dir)?;
    fmt.args(["fmt", "--manifest-path", "Cargo.toml"]);
    run_checked(&mut fmt).context("failed to format generated plugin scaffold")
}

#[derive(Clone, Debug)]
struct ScaffoldSpec {
    kind: PluginKindArg,
    plugin_id: String,
    crate_name: String,
}

impl ScaffoldSpec {
    fn new(kind: PluginKindArg, name: &str) -> Result<Self> {
        let plugin_id = normalize_plugin_name(name)?;
        let crate_name = format!("{}_{}", plugin_id.replace('-', "_"), kind.crate_suffix());
        Ok(Self {
            kind,
            plugin_id,
            crate_name,
        })
    }

    fn render_lib_rs(&self) -> Result<String> {
        render_template(
            "lib.rs",
            LIB_RS_TEMPLATE,
            &[
                ("plugin_id", &self.plugin_id),
                ("sdk_imports", self.kind.sdk_imports_template()),
                (
                    "provider_variant",
                    &render_template(
                        "provider variant",
                        self.kind.provider_template(),
                        &[("plugin_id", &self.plugin_id)],
                    )?,
                ),
                ("family_exports", self.kind.exports_template()),
            ],
        )
    }

    fn render_manifest(&self) -> String {
        let mut document = DocumentMut::new();
        document["package"]["name"] = value(self.crate_name.as_str());
        document["package"]["version"] = value("0.1.0");
        document["package"]["edition"] = value("2024");

        let mut crate_types = Array::new();
        crate_types.push("cdylib");
        document["lib"]["crate-type"] = Item::Value(crate_types.into());

        document["dependencies"]["extism-pdk"] = value("1");
        document["dependencies"]["scryer-plugin-sdk"] = value(SDK_VERSION);
        document["dependencies"]["serde_json"] = value("1");
        document.to_string()
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

fn render_template(
    template_name: &str,
    template: &str,
    replacements: &[(&str, &str)],
) -> Result<String> {
    let mut rendered = template.to_string();
    for (key, value) in replacements {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }

    if rendered.contains("{{") || rendered.contains("}}") {
        bail!("template {template_name} has unreplaced placeholders");
    }

    Ok(rendered.trim_start().to_string())
}

impl PluginKindArg {
    fn directory(self) -> &'static str {
        match self {
            Self::Indexer => "indexers",
            Self::DownloadClient => "download_clients",
            Self::Notification => "notifications",
            Self::Subtitle => "subtitles",
        }
    }

    fn crate_suffix(self) -> &'static str {
        match self {
            Self::Indexer => "indexer",
            Self::DownloadClient => "download_client",
            Self::Notification => "notification",
            Self::Subtitle => "subtitle_provider",
        }
    }

    fn sdk_imports_template(self) -> &'static str {
        match self {
            Self::Indexer => INDEXER_IMPORTS_TEMPLATE,
            Self::DownloadClient => DOWNLOAD_CLIENT_IMPORTS_TEMPLATE,
            Self::Notification => NOTIFICATION_IMPORTS_TEMPLATE,
            Self::Subtitle => SUBTITLE_IMPORTS_TEMPLATE,
        }
    }

    fn provider_template(self) -> &'static str {
        match self {
            Self::Indexer => INDEXER_PROVIDER_TEMPLATE,
            Self::DownloadClient => DOWNLOAD_CLIENT_PROVIDER_TEMPLATE,
            Self::Notification => NOTIFICATION_PROVIDER_TEMPLATE,
            Self::Subtitle => SUBTITLE_PROVIDER_TEMPLATE,
        }
    }

    fn exports_template(self) -> &'static str {
        match self {
            Self::Indexer => INDEXER_EXPORTS_TEMPLATE,
            Self::DownloadClient => DOWNLOAD_CLIENT_EXPORTS_TEMPLATE,
            Self::Notification => NOTIFICATION_EXPORTS_TEMPLATE,
            Self::Subtitle => SUBTITLE_EXPORTS_TEMPLATE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_task_context() -> (tempfile::TempDir, TaskContext) {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let repo_dir = temp_dir.path().join("repo");
        fs::create_dir_all(&repo_dir).expect("create repo dir");
        (
            temp_dir,
            TaskContext {
                repo_root: repo_dir,
            },
        )
    }

    #[test]
    fn render_lib_rs_contains_expected_descriptor_variants() {
        let cases = [
            (
                PluginKindArg::Indexer,
                "ProviderDescriptor::Indexer(IndexerDescriptor {",
                "pub fn scryer_indexer_search",
            ),
            (
                PluginKindArg::DownloadClient,
                "ProviderDescriptor::DownloadClient(DownloadClientDescriptor {",
                "pub fn scryer_download_add",
            ),
            (
                PluginKindArg::Notification,
                "ProviderDescriptor::Notification(NotificationDescriptor {",
                "pub fn scryer_notification_send",
            ),
            (
                PluginKindArg::Subtitle,
                "ProviderDescriptor::Subtitle(SubtitleDescriptor {",
                "pub fn scryer_subtitle_search",
            ),
        ];

        for (kind, provider_variant, exported_fn) in cases {
            let spec = ScaffoldSpec::new(kind, "Example Plugin").expect("build scaffold spec");
            let lib_rs = spec.render_lib_rs().expect("render lib.rs");
            assert!(
                lib_rs.contains(provider_variant),
                "{kind:?} provider variant missing"
            );
            assert!(lib_rs.contains(exported_fn), "{kind:?} exports missing");
            assert!(
                !lib_rs.contains("{{") && !lib_rs.contains("}}"),
                "{kind:?} left template placeholders behind"
            );
        }
    }

    #[test]
    fn render_manifest_contains_expected_package_contract() {
        let spec =
            ScaffoldSpec::new(PluginKindArg::Notification, "Webhook Example").expect("build spec");
        let manifest = spec.render_manifest();
        let document = manifest.parse::<DocumentMut>().expect("parse manifest");

        assert_eq!(
            document["package"]["name"].as_str(),
            Some("webhook_example_notification")
        );
        assert_eq!(document["package"]["version"].as_str(), Some("0.1.0"));
        assert_eq!(document["package"]["edition"].as_str(), Some("2024"));
        assert_eq!(document["lib"]["crate-type"][0].as_str(), Some("cdylib"));
        assert_eq!(document["dependencies"]["extism-pdk"].as_str(), Some("1"));
        assert_eq!(
            document["dependencies"]["scryer-plugin-sdk"].as_str(),
            Some(SDK_VERSION)
        );
        assert_eq!(document["dependencies"]["serde_json"].as_str(), Some("1"));
    }

    #[test]
    fn scaffold_plugin_writes_files_and_formats_generated_crate() {
        let (temp_dir, ctx) = temp_task_context();
        fs::write(
            ctx.repo_root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.94\"\n",
        )
        .expect("write toolchain file");

        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let plugin_name = format!("Scaffold Test {seed}");
        let plugin_dir = scaffold_plugin(
            &ctx,
            &ctx.repo_root,
            PluginKindArg::Notification,
            &plugin_name,
        )
        .expect("scaffold plugin");

        let cargo_toml = plugin_dir.join("Cargo.toml");
        let lib_rs = plugin_dir.join("src/lib.rs");
        assert!(cargo_toml.is_file(), "Cargo.toml should exist");
        assert!(lib_rs.is_file(), "src/lib.rs should exist");

        let mut fmt_check = repo_cargo_command_in(&ctx, &plugin_dir).expect("cargo fmt command");
        fmt_check.args(["fmt", "--check", "--manifest-path", "Cargo.toml"]);
        run_checked(&mut fmt_check).expect("generated crate should be fmt clean");

        drop(temp_dir);
    }
}
