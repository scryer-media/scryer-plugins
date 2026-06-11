use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, DownloadClientCapabilities, DownloadClientDescriptor,
    DownloadControlAction, DownloadInputKind, DownloadIsolationMode, DownloadItemState,
    PluginCompletedDownload, PluginDescriptor, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;

#[derive(Debug, Clone)]
struct PneumaticConfig {
    nzb_folder: String,
    strm_folder: String,
}

#[derive(Default, Deserialize)]
struct AddRequest {
    #[serde(default)]
    source: AddSource,
    #[serde(default)]
    release: AddRelease,
}

#[derive(Default, Deserialize)]
struct AddSource {
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    nzb_bytes_base64: Option<String>,
    #[serde(default)]
    nzb_file_name: Option<String>,
    #[serde(default)]
    source_title: Option<String>,
}

#[derive(Default, Deserialize)]
struct AddRelease {
    #[serde(default)]
    release_title: Option<String>,
    #[serde(default)]
    season_pack: Option<bool>,
}

fn plugin_error<T>(code: PluginErrorCode, public_message: impl Into<String>) -> PluginResult<T> {
    PluginResult::Err(PluginError {
        code,
        public_message: public_message.into(),
        debug_message: None,
        retry_after_seconds: None,
    })
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "pneumatic".to_string(),
        name: "Pneumatic".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "pneumatic".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![DownloadInputKind::Nzb, DownloadInputKind::NzbUrl],
            isolation_modes: vec![DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
                remove: false,
                remove_with_data: false,
                mark_imported: false,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: false,
                start_paused: false,
                force_start: false,
                per_download_directory: false,
                host_fs_required: true,
                test_connection: true,
                torrent: None,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: AddRequest = serde_json::from_str(&input)?;
    if request.release.season_pack.unwrap_or(false) {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Unsupported,
            "Full season releases are not supported with Pneumatic",
        ))?);
    }

    let config = PneumaticConfig::from_extism()?;
    fs::create_dir_all(&config.nzb_folder)
        .map_err(|error| Error::msg(format!("failed to create NZB folder: {error}")))?;
    fs::create_dir_all(&config.strm_folder)
        .map_err(|error| Error::msg(format!("failed to create STRM folder: {error}")))?;

    let title = clean_file_name(
        request
            .source
            .nzb_file_name
            .as_deref()
            .map(|value| value.trim_end_matches(".nzb"))
            .or(request.release.release_title.as_deref())
            .or(request.source.source_title.as_deref())
            .unwrap_or("download"),
    );
    let nzb_path = Path::new(&config.nzb_folder).join(format!("{title}.nzb"));
    write_file(&nzb_path, &nzb_payload(&request)?)?;
    let strm_path = write_strm_file(&config, &title, &nzb_path)?;
    let id = strm_path.to_string_lossy().to_string();

    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: id,
            info_hash: None,
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    scryer_download_list_queue_inner()
}

fn scryer_download_list_queue_inner() -> FnResult<String> {
    let config = PneumaticConfig::from_extism()?;
    let items = scan_strm_folder(&config)
        .into_iter()
        .map(entry_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    scryer_download_list_queue_inner()
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = PneumaticConfig::from_extism()?;
    let downloads = scan_strm_folder(&config)
        .into_iter()
        .map(entry_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    match request.action {
        DownloadControlAction::Remove
        | DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => Ok(serde_json::to_string(&plugin_error::<()>(
            PluginErrorCode::Unsupported,
            "Pneumatic does not support download control actions",
        ))?),
    }
}

#[plugin_fn]
pub fn scryer_download_mark_imported(_input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&_input)?;
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = PneumaticConfig::from_extism()?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: None,
            is_localhost: Some(true),
            remote_output_roots: if config.strm_folder.is_empty() {
                Vec::new()
            } else {
                vec![config.strm_folder]
            },
            removes_completed_downloads: Some(false),
            sorting_mode: Some("strm-folder".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = PneumaticConfig::from_extism()?;
    ensure_directory(&config.nzb_folder, "NZB Folder")?;
    ensure_directory(&config.strm_folder, "STRM Folder")?;
    Ok(serde_json::to_string(&PluginResult::Ok("ok".to_string()))?)
}

impl PneumaticConfig {
    fn from_extism() -> Result<Self, Error> {
        Ok(Self {
            nzb_folder: config_value("nzb_folder").unwrap_or_default(),
            strm_folder: config_value("strm_folder").unwrap_or_default(),
        })
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "nzb_folder",
            "NZB Folder",
            ConfigFieldType::String,
            true,
            None,
            Some("Folder where Pneumatic .nzb files are written."),
        ),
        field(
            "strm_folder",
            "STRM Folder",
            ConfigFieldType::String,
            true,
            None,
            Some("Folder where Pneumatic .strm files are written and scanned."),
        ),
    ]
}

fn nzb_payload(request: &AddRequest) -> Result<Vec<u8>, Error> {
    if let Some(raw) = request
        .source
        .nzb_bytes_base64
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return STANDARD
            .decode(raw)
            .map_err(|error| Error::msg(format!("invalid nzb_bytes_base64: {error}")));
    }

    let Some(url) = request
        .source
        .download_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Err(Error::msg("Pneumatic requires an NZB payload or NZB URL"));
    };
    get_external_bytes(url)
}

fn get_external_bytes(url: &str) -> Result<Vec<u8>, Error> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-pneumatic-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| Error::msg(format!("NZB URL request failed: {error}")))?;
    let status = response.status_code();
    let body = response.body();
    if status >= 400 {
        return Err(Error::msg(format!(
            "NZB URL returned HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    if body.is_empty() {
        return Err(Error::msg("NZB URL returned an empty response body"));
    }
    Ok(body)
}

fn write_strm_file(
    config: &PneumaticConfig,
    title: &str,
    nzb_path: &Path,
) -> Result<PathBuf, Error> {
    let nzb_path_string = nzb_path.to_string_lossy();
    let contents = format!(
        "plugin://plugin.program.pneumatic/?mode=strm&type=add_file&nzb={nzb_path_string}&nzbname={title}"
    );
    let path = Path::new(&config.strm_folder).join(format!("{title}.strm"));
    write_file(&path, contents.as_bytes())?;
    Ok(path)
}

fn write_file(path: &Path, content: &[u8]) -> Result<(), Error> {
    let mut file = fs::File::create(path)
        .map_err(|error| Error::msg(format!("failed to open file: {error}")))?;
    file.write_all(content)
        .map_err(|error| Error::msg(format!("failed to write file: {error}")))
}

fn scan_strm_folder(config: &PneumaticConfig) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(&config.strm_folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("strm"))
        })
        .collect()
}

fn entry_to_item(path: PathBuf) -> PluginDownloadItem {
    let id = path.to_string_lossy().to_string();
    let title = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let size = path_size(&path);
    PluginDownloadItem {
        client_item_id: id.clone(),
        download_id: None,
        info_hash: None,
        title,
        state: DownloadItemState::Completed,
        message: None,
        category: None,
        remote_output_path: Some(id.clone()),
        torrent: None,
        total_size_bytes: Some(size),
        remaining_size_bytes: Some(0),
        eta_seconds: Some(0),
        progress_percent: Some(100),
        can_move_files: Some(true),
        can_remove: Some(false),
        removed: Some(false),
        raw_state: Some("completed".to_string()),
        completed_at: None,
    }
}

fn entry_to_completed(path: PathBuf) -> PluginCompletedDownload {
    let id = path.to_string_lossy().to_string();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    PluginCompletedDownload {
        client_item_id: id.clone(),
        download_id: None,
        info_hash: None,
        name,
        dest_dir: id.clone(),
        category: None,
        output_kind: Some(PluginDownloadOutputKind::File),
        content_paths: vec![id],
        size_bytes: Some(path_size(&path)),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn path_size(path: &Path) -> i64 {
    path.metadata()
        .map(|meta| meta.len() as i64)
        .unwrap_or_default()
}

fn ensure_directory(path: &str, label: &str) -> Result<(), Error> {
    if path.trim().is_empty() {
        return Err(Error::msg(format!("{label} is required")));
    }
    let metadata = fs::metadata(path)
        .map_err(|error| Error::msg(format!("{label} is not accessible: {error}")))?;
    if !metadata.is_dir() {
        return Err(Error::msg(format!("{label} is not a directory")));
    }
    Ok(())
}

fn clean_file_name(value: &str) -> String {
    let value = value.replace(": ", " - ").replace(':', "-");
    let cleaned = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' => '+',
            '?' => '!',
            '*' => '-',
            '"' | '<' | '>' | '|' => '\0',
            _ => ch,
        })
        .filter(|ch| *ch != '\0')
        .collect::<String>()
        .trim_start_matches([' ', '.'])
        .trim_end_matches(' ')
        .to_string();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: Default::default(),
        host_binding: None,
        role: None,
        options: vec![],
        help_text: help_text.map(str::to_string),
    }
}
