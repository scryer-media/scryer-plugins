use base64::{Engine as _, engine::general_purpose::STANDARD};
use roxmltree::{Document, Node};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadInputKind, DownloadIsolationMode,
    DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use sha1::{Digest, Sha1};

/// Sonarr's floor for this client (`Aria2.cs:237`). aria2 1.34.0 is also the
/// first release whose `aria2.tellStatus` this plugin's field set is checked
/// against; every key in [`STATUS_KEYS`] predates it.
const MIN_SUPPORTED_VERSION: &str = "1.34.0";

/// The `keys` argument aria2 accepts on `tellStatus`/`tellActive`/`tellWaiting`/
/// `tellStopped` ("RPC INTERFACE", aria2 manual): a status response is trimmed
/// to exactly these members. Sonarr asks for everything and therefore drags a
/// `bitfield` — two hex characters per eight pieces — through every poll of
/// every torrent. aria2 matches requested keys with
/// `requested_key(keys, k) { keys.empty() || find(...) }`
/// (`src/RpcMethodImpl.cc`), so an entry it does not know is ignored rather
/// than rejected, and asking is safe on any version.
const STATUS_KEYS: &[&str] = &[
    "gid",
    "status",
    "totalLength",
    "completedLength",
    "uploadLength",
    "downloadSpeed",
    "uploadSpeed",
    "infoHash",
    "seeder",
    "errorCode",
    "errorMessage",
    "followedBy",
    "files",
    "bittorrent",
    "verifiedLength",
];

/// aria2 numbers a download with a 64-bit GID rendered as 16 hex characters
/// ("GID" in the aria2 manual). A BitTorrent info hash is 40. The two are
/// therefore never confusable, which is what lets `resolve_gid` take a
/// `tellStatus` fast path instead of listing everything.
const GID_HEX_LEN: usize = 16;

#[derive(Debug, Clone)]
struct Aria2Config {
    rpc_url: String,
    secret_token: String,
    directory: String,
}

#[derive(Debug, Clone, Default)]
struct Aria2Status {
    bittorrent_name: Option<String>,
    info_hash: Option<String>,
    completed_length: i64,
    download_speed: i64,
    upload_speed: i64,
    files: Vec<String>,
    followed_by: Vec<String>,
    gid: String,
    status: String,
    total_length: i64,
    upload_length: i64,
    error_code: Option<String>,
    error_message: Option<String>,
    /// `true` when the status carried a `bittorrent` member or an `infoHash`.
    /// aria2 emits both only for BitTorrent downloads (`gatherProgress` in
    /// `src/RpcMethodImpl.cc`), so this is how a torrent is told apart from the
    /// plain HTTP/FTP download `aria2.addUri` also accepts.
    is_torrent: bool,
    /// aria2's own "the local endpoint is a seeder" flag (BitTorrent only).
    seeder: Option<bool>,
    /// Present only while aria2 is hash-checking this download.
    verified_length: Option<i64>,
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// `Err(Error::msg(..))` reaches the host as `PluginErrorCode::Temporary`, so
/// every failure this plugin can name carries its own code instead
/// (`00-common.md` rule 4). The distinctions mirror Sonarr's two outcomes for
/// aria2 — a version validation failure and a `Host` "unable to connect"
/// (`Aria2.cs:231-258`) — plus the XML-RPC fault text its proxy raises as a
/// `DownloadClientException` (`Aria2Proxy.cs:167-174`).
fn plugin_error(code: PluginErrorCode, public_message: impl Into<String>) -> PluginError {
    PluginError {
        code,
        public_message: public_message.into(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}

fn detailed_error(
    code: PluginErrorCode,
    public_message: impl Into<String>,
    debug_message: impl Into<String>,
) -> PluginError {
    PluginError {
        debug_message: Some(debug_message.into()),
        ..plugin_error(code, public_message)
    }
}

fn respond<T: serde::Serialize>(result: Result<T, PluginError>) -> FnResult<String> {
    let result = match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => PluginResult::Err(error),
    };
    Ok(serde_json::to_string(&result)?)
}

fn parse_request<T: serde::de::DeserializeOwned>(input: &str) -> Result<T, PluginError> {
    serde_json::from_str(input).map_err(|error| {
        detailed_error(
            PluginErrorCode::Permanent,
            "Scryer sent a request this Aria2 plugin could not read.",
            error.to_string(),
        )
    })
}

fn truncate(value: &str) -> String {
    const LIMIT: usize = 512;
    if value.len() <= LIMIT {
        return value.to_string();
    }
    let mut end = LIMIT;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// aria2 answers a well-formed RPC call with HTTP 200 even when the call
/// itself failed, so a non-2xx here is about the endpoint, not the download.
/// The host runs plugin HTTP with redirects disabled, which is how a reverse
/// proxy bouncing `/rpc` to a login page arrives as a 3xx instead of an
/// unparseable body.
fn classify_http_status(status: u16, location: Option<&str>, body: &str) -> Option<PluginError> {
    match status {
        200..=299 => None,
        300..=399 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            match location.map(str::trim).filter(|value| !value.is_empty()) {
                Some(location) => {
                    format!(
                        "Aria2's RPC endpoint redirected to {location}; check host, port and RPC path."
                    )
                }
                None => {
                    "Aria2's RPC endpoint redirected the request; check host, port and RPC path."
                        .to_string()
                }
            },
        )),
        // aria2 itself never answers 401/403 — it reports a bad secret token as
        // an XML-RPC fault (see `classify_fault`) — so these come from a proxy
        // in front of it. Either way the credentials are what is wrong.
        401 | 403 => Some(detailed_error(
            PluginErrorCode::AuthFailed,
            "Aria2's RPC endpoint rejected the request as unauthorized.",
            truncate(body),
        )),
        404 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            "No Aria2 RPC endpoint was found at this address; check the RPC path.",
        )),
        500..=599 => Some(detailed_error(
            PluginErrorCode::Temporary,
            format!("Aria2 returned HTTP {status}."),
            truncate(body),
        )),
        _ => Some(detailed_error(
            PluginErrorCode::Permanent,
            format!("Aria2 returned HTTP {status}."),
            truncate(body),
        )),
    }
}

/// The host hands transport failures back as a string, so classification is by
/// substring. This is the closest this surface gets to the exception Sonarr
/// turns into its `Host` "unable to connect" validation failure
/// (`Aria2.cs:247-255`).
fn classify_transport_error(detail: &str) -> PluginError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timeout") || lowered.contains("timed out") {
        detailed_error(
            PluginErrorCode::Temporary,
            "Aria2 did not answer in time.",
            detail,
        )
    } else if lowered.contains("certificate")
        || lowered.contains("tls")
        || lowered.contains("ssl")
        || lowered.contains("trust")
    {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Aria2: certificate validation failed.",
            detail,
        )
    } else {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Aria2, please check your settings.",
            detail,
        )
    }
}

/// aria2 stamps **every** method failure with `faultCode` 1
/// (`RpcMethod::createErrorResponse` in `src/RpcMethod.cc` puts
/// `Integer::g(1)` under `faultCode`), so the code carries no information and
/// the fault string is the only discriminator there is. A rejected
/// `--rpc-secret` token is `DL_ABORT_EX("Unauthorized")` from
/// `RpcMethod::authorize`, which is the one fault worth its own error code.
///
/// Sonarr flattens all of these into one `DownloadClientException`
/// (`Aria2Proxy.cs:173`); Scryer's contract wants the wrong-token case
/// distinguishable from a bad request.
fn classify_fault(code: &str, message: &str) -> PluginError {
    let detail = format!("Aria2 returned error code {code}: {message}");
    if message.to_ascii_lowercase().contains("unauthorized") {
        return detailed_error(
            PluginErrorCode::AuthFailed,
            "Aria2 rejected the secret token.",
            detail,
        );
    }
    detailed_error(
        PluginErrorCode::Permanent,
        format!("Aria2 rejected the request: {message}"),
        detail,
    )
}

pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "aria2".to_string(),
        name: "Aria2".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "aria2".to_string(),
            provider_aliases: vec!["aria2c".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                pause: true,
                resume: true,
                remove: true,
                // aria2 has no delete-files call (aria2 issue #728), and the
                // core clamps the flag rather than sending a request the plugin
                // would refuse (`download_client_adapter.rs:389-417`). See the
                // status warning for what that means on disk.
                remove_with_data: false,
                // aria2 has no label, tag, category or view to write back to,
                // so there is no non-destructive handoff for it to perform and
                // nothing for a destructive one to do that the core's seeding
                // gate does not already own.
                mark_imported: false,
                mark_imported_non_destructive: false,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                // `seed-ratio` / `seed-time` are per-download options of
                // `aria2.addUri` / `aria2.addTorrent` (aria2 manual, "Input
                // File"). Sonarr never sets them for aria2; Scryer routes a
                // seeding goal to every client that can carry one.
                seed_limits: true,
                start_paused: false,
                force_start: false,
                per_download_directory: true,
                host_fs_required: false,
                test_connection: true,
                torrent: Some(DownloadTorrentCapabilities {
                    supported_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentFile,
                    ],
                    preferred_sources: vec![
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentFile,
                    ],
                    isolation_modes: vec![DownloadIsolationMode::Directory],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: true,
                    // aria2 stops seeding when a limit is met and moves the
                    // download to `complete`; it never drops the entry.
                    removes_on_seed_limit: false,
                    supports_start_paused: false,
                    supports_force_start: false,
                    supports_sequential_download: false,
                    supports_first_last_piece_priority: false,
                    supports_content_layout: false,
                    supports_skip_checking: false,
                    supports_auto_management: false,
                    supports_post_import_isolation: false,
                    reports_content_paths: true,
                    ..DownloadTorrentCapabilities::default()
                }),
                // SDK 3.10 addition. `false` is the SDK's own default and therefore exactly
                // what this client's pre-3.10 descriptor already meant to a 3.10 host;
                // advertising category-scoped feedback would be a behaviour change, not a
                // transport one, so it stays off across the component migration.
                category_scoped_feedback: false,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add(&input))
}

fn add(input: &str) -> Result<PluginDownloadClientAddResponse, PluginError> {
    let request: PluginDownloadClientAddRequest = parse_request(input)?;
    let config = Aria2Config::from_host();
    let options = add_options(&config, &request);

    let gid = if let Some(bytes_base64) = request.source.torrent_bytes_base64.as_deref() {
        let torrent_bytes = STANDARD.decode(bytes_base64).map_err(|error| {
            detailed_error(
                PluginErrorCode::Permanent,
                "Scryer sent torrent bytes Aria2 could not accept.",
                format!("invalid torrent_bytes_base64: {error}"),
            )
        })?;
        call_string(
            &config,
            "aria2.addTorrent",
            &[
                xml_base64(&torrent_bytes),
                // aria2's second parameter is an array of URIs and has to be
                // present whenever options follow it (Sonarr notes the same,
                // `Aria2Proxy.cs:116-117`).
                xml_array(Vec::new()),
                xml_struct(&options),
            ],
        )?
    } else if let Some(source) = source_url(&request) {
        call_string(
            &config,
            "aria2.addUri",
            &[xml_array(vec![xml_string(&source)]), xml_struct(&options)],
        )?
    } else {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "download source is missing",
        ));
    };

    // The item id is the info hash whenever one can exist. The GID aria2 just
    // returned belongs to the *metadata* download for a magnet or a torrent
    // URL; aria2 replaces it with a new GID (`followedBy`) once the metainfo
    // resolves, and this plugin hides metadata rows the way Sonarr does
    // (`Aria2.cs:85`), so a GID identity would leave the download invisible in
    // every later listing. Sonarr sidesteps this by knowing the hash up front
    // (`Aria2.cs:40-57` returns `hash`, never the GID).
    let hash = derive_info_hash(&request).or_else(|| resolve_hash_after_add(&config, &gid));
    let client_item_id = hash.clone().unwrap_or_else(|| gid.clone());
    Ok(PluginDownloadClientAddResponse {
        client_item_id,
        info_hash: hash,
    })
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = Aria2Config::from_host();
    Ok(list_torrents(&config)?
        .into_iter()
        .filter(is_visible_download)
        .map(torrent_to_item)
        .collect())
}

/// aria2 keeps no failed history the queue listing does not already carry: an
/// `error` result lives in `tellStopped`, which `list_torrents` already reads.
/// The PDK bridge calls `list_queue` **and** `list_history` on every queue poll
/// and merges the `Failed`/`Error` rows
/// (`pdk/scryer-plugin-pdk/src/download_client_bridge.rs:166-203`), so
/// answering here would run three more XML-RPC calls per poll to re-deliver
/// rows the queue just produced.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(Ok::<Vec<PluginDownloadItem>, PluginError>(Vec::new()))
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, PluginError> {
    let config = Aria2Config::from_host();
    Ok(list_torrents(&config)?
        .into_iter()
        .filter(is_visible_download)
        .filter(|torrent| torrent.status == "complete")
        .map(torrent_to_completed)
        .collect())
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientControlRequest = parse_request(input)?;

    // Answered before the daemon is contacted: aria2 has no notion of forcing a
    // download past the queue, and no configuration changes that.
    if matches!(request.action, DownloadControlAction::ForceStart) {
        return Err(plugin_error(
            PluginErrorCode::Unsupported,
            "Aria2 does not support force_start through this plugin",
        ));
    }

    let config = Aria2Config::from_host();
    let Some(gid) = resolve_gid(&config, &request.client_item_id)? else {
        // Sonarr logs and returns for a removal it cannot find
        // (`Aria2.cs:158-162`) rather than raising, and the core treats an
        // absent item as already gone. Pause/resume of something that is not
        // there is still a real error.
        if matches!(request.action, DownloadControlAction::Remove) {
            return Ok(());
        }
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "download item was not found",
        ));
    };

    match request.action {
        DownloadControlAction::Pause => {
            call_string(&config, "aria2.pause", &[xml_string(&gid)])?;
        }
        DownloadControlAction::Resume => {
            call_string(&config, "aria2.unpause", &[xml_string(&gid)])?;
        }
        DownloadControlAction::Remove => {
            remove_download(&config, &gid)?;
        }
        DownloadControlAction::ForceStart => unreachable!("handled above"),
    }

    Ok(())
}

/// aria2 splits removal in two: a still-running download is killed with
/// `forceRemove` (which answers with the GID), a finished or failed one has its
/// *result* dropped with `removeDownloadResult` (which answers `"OK"`). Sonarr
/// makes the same split and checks the same two answers
/// (`Aria2.cs:166-183`, `Aria2Proxy.cs:131-147`).
fn remove_download(config: &Aria2Config, gid: &str) -> Result<(), PluginError> {
    let raw_status = try_tell_status(config, gid).map(|status| status.status);
    let (method, answers_ok) = removal_call(raw_status.as_deref());
    let expected = if answers_ok {
        "OK".to_string()
    } else {
        gid.to_string()
    };
    let answer = call_string(config, method, &[xml_string(gid)])?;
    if answer != expected {
        return Err(detailed_error(
            PluginErrorCode::Temporary,
            "Aria2 did not confirm the removal.",
            format!("{method} answered {answer:?}, expected {expected:?}"),
        ));
    }
    Ok(())
}

/// Which removal call a download in `raw_status` takes, and whether it answers
/// `"OK"` rather than echoing the GID. A status aria2 would not give us (the
/// download is already gone) takes the running-download branch, which is what
/// Sonarr's `null` status would also do.
fn removal_call(raw_status: Option<&str>) -> (&'static str, bool) {
    if raw_status.is_some_and(|status| matches!(status, "complete" | "error" | "removed")) {
        ("aria2.removeDownloadResult", true)
    } else {
        ("aria2.forceRemove", false)
    }
}

/// aria2 has no label, tag, category or view, so there is nothing to write back
/// to it after an import.
///
/// The descriptor says so (`mark_imported_non_destructive: false`), which is
/// what the core reads before it schedules a handoff
/// (`download_client_adapter.rs:1450-1467`), and the function table leaves the
/// non-destructive slot empty so the bridge answers `Ok(())` itself. This body
/// exists only because the legacy table requires the destructive slot to be
/// filled; the core has no caller for it.
///
/// This replaces a `post_import_action = remove` option that called
/// `aria2.removeDownloadResult` from here. Removing a finished download is the
/// core's decision through the seeding gate, never the plugin's
/// (`00-common.md` rule 3), and Sonarr's aria2 client has no post-import action
/// at all. The config key is retired; a stored value of either `retain` or
/// `remove` is simply no longer read, and both are no-ops.
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = match parse_request(&input) {
        Ok(request) => request,
        Err(error) => return respond(Err::<(), PluginError>(error)),
    };
    respond(Ok::<(), PluginError>(()))
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(client_status())
}

fn client_status() -> Result<PluginDownloadClientStatus, PluginError> {
    let config = Aria2Config::from_host();
    let version = get_version(&config)?;
    let globals = get_globals(&config)?;
    let mut roots = Vec::new();
    if let Some(dir) = globals.get("dir").filter(|value| !value.is_empty()) {
        roots.push(dir.clone());
    }
    if !config.directory.is_empty() && !roots.iter().any(|root| root == &config.directory) {
        roots.push(config.directory.clone());
    }
    Ok(PluginDownloadClientStatus {
        version: Some(version),
        is_localhost: Some(is_localhost_url(&config.rpc_url)),
        remote_output_roots: roots,
        removes_completed_downloads: Some(false),
        // aria2 has no category or sorting scheme of its own; claiming one
        // would put a meaningless string in front of the operator.
        sorting_mode: None,
        warnings: vec![
            "Aria2 has no delete-files RPC call (aria2 issue #728), so Scryer removes only \
             the download entry and leaves the downloaded files on disk for you to clean up."
                .to_string(),
        ],
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

fn test_connection() -> Result<String, PluginError> {
    let config = Aria2Config::from_host();
    let version = get_version(&config)?;
    if version_is_older_than(&version, MIN_SUPPORTED_VERSION) {
        // Sonarr's `DownloadClientValidationErrorVersion`, which names both the
        // required and the reported version (`Aria2.cs:237-244`). It is a
        // settings problem, not a transient one.
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("Aria2 {version} is older than the required version {MIN_SUPPORTED_VERSION}."),
        ));
    }
    Ok(version)
}

impl Aria2Config {
    fn from_host() -> Self {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "6800".to_string());
        let rpc_path = config_value("rpc_path").unwrap_or_else(|| "/rpc".to_string());
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        Self {
            rpc_url: format!(
                "{scheme}://{host}:{port}/{}",
                rpc_path.trim_start_matches('/')
            ),
            secret_token: config_value("secret_token").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
        }
    }

    fn token_param(&self) -> String {
        xml_string(&format!("token:{}", self.secret_token))
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "host",
            "Host",
            ConfigFieldType::String,
            true,
            Some("localhost"),
            None,
        ),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("6800"),
            None,
        ),
        connection_field("rpc_path", "XML-RPC Path", true, Some("/rpc"), None),
        field(
            "use_ssl",
            "Use SSL",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "secret_token",
            "Secret Token",
            ConfigFieldType::Password,
            false,
            Some("MySecretToken"),
            Some("The --rpc-secret token the Aria2 daemon was started with."),
        ),
        field(
            "directory",
            "Directory",
            ConfigFieldType::Path,
            false,
            None,
            Some("Download directory to use when Scryer does not route one for the download."),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Add options
// ---------------------------------------------------------------------------

/// The per-download options handed to `aria2.addUri` / `aria2.addTorrent`.
///
/// `dir` is Sonarr's only option (`Aria2Proxy.cs:103-106`) and it can use only
/// the configured one; Scryer routes a directory per download, which is what
/// `per_download_directory` advertises. `seed-ratio` and `seed-time` are
/// documented per-download options (aria2 manual, "Input File") that Sonarr
/// never sets — `seed-time` is in **minutes**, so a goal in seconds is
/// converted here.
fn add_options(
    config: &Aria2Config,
    request: &PluginDownloadClientAddRequest,
) -> Vec<(String, String)> {
    let mut options = Vec::new();

    if let Some(directory) = request
        .routing
        .download_directory
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| (!config.directory.is_empty()).then(|| config.directory.clone()))
    {
        options.push(("dir".to_string(), directory));
    }

    let torrent = request.torrent.as_ref();
    let ratio = torrent
        .and_then(|torrent| torrent.seed_goal_ratio)
        .or(request.release.seed_goal_ratio)
        .filter(|ratio| ratio.is_finite() && *ratio >= 0.0);
    let seconds = torrent
        .and_then(|torrent| torrent.seed_goal_seconds)
        .or(request.release.seed_goal_seconds)
        .filter(|seconds| *seconds > 0);

    // Scryer's gate treats a ratio goal of 0 as met the moment the payload is
    // complete (`seeding_gate.rs::scryer_goal_is_met`: `observed >= goal`,
    // OR-semantics across the two axes). aria2 reads `seed-ratio=0.0` as the
    // opposite — "seed regardless of share ratio" (aria2 manual,
    // `--seed-ratio`) — so a zero ratio is expressed as `seed-time=0`, which
    // the manual documents as "disables seeding after download completed".
    if ratio == Some(0.0) {
        options.push(("seed-time".to_string(), "0".to_string()));
        return options;
    }
    if let Some(ratio) = ratio {
        options.push(("seed-ratio".to_string(), format_seed_ratio(ratio)));
    }
    if let Some(seconds) = seconds {
        options.push(("seed-time".to_string(), format_seed_minutes(seconds)));
    }

    options
}

/// aria2 parses `seed-ratio` as a double; render it without an exponent and
/// without a trailing `.0` that would read oddly in the daemon's session file.
fn format_seed_ratio(ratio: f64) -> String {
    let rendered = format!("{ratio:.3}");
    let trimmed = rendered.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `--seed-time` is "seeding time in (fractional) minutes" (aria2 manual).
fn format_seed_minutes(seconds: i64) -> String {
    format_seed_ratio(seconds as f64 / 60.0)
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

fn user_agent() -> String {
    format!("scryer-aria2-plugin/{}", env!("CARGO_PKG_VERSION"))
}

fn call_document(
    config: &Aria2Config,
    method: &str,
    params: &[String],
) -> Result<String, PluginError> {
    let mut all_params = vec![config.token_param()];
    all_params.extend_from_slice(params);
    let body = format!(
        r#"<?xml version="1.0"?><methodCall><methodName>{}</methodName><params>{}</params></methodCall>"#,
        xml_escape(method),
        all_params
            .iter()
            .map(|param| format!("<param><value>{param}</value></param>"))
            .collect::<Vec<_>>()
            .join("")
    );
    let request = HttpRequest::new(&config.rpc_url)
        .with_method("POST")
        .with_header("Content-Type", "text/xml")
        .with_header("User-Agent", &user_agent());
    let response = http::request::<Vec<u8>>(&request, Some(body.into_bytes()))
        .map_err(|error| classify_transport_error(&error.to_string()))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if let Some(error) = classify_http_status(status, response.header("Location"), &text) {
        return Err(error);
    }
    check_fault(&text)?;
    Ok(text)
}

fn call_string(
    config: &Aria2Config,
    method: &str,
    params: &[String],
) -> Result<String, PluginError> {
    let xml = call_document(config, method, params)?;
    let doc = parse_document(&xml)?;
    let value = first_response_value(&doc)
        .ok_or_else(|| malformed("the response carried no XML-RPC value"))?;
    Ok(node_text(value).unwrap_or_default())
}

fn parse_document(xml: &str) -> Result<Document<'_>, PluginError> {
    Document::parse(xml).map_err(|error| {
        detailed_error(
            PluginErrorCode::Permanent,
            "Aria2 returned a response Scryer could not parse.",
            format!("invalid XML: {error}"),
        )
    })
}

fn malformed(detail: &str) -> PluginError {
    detailed_error(
        PluginErrorCode::Permanent,
        "Aria2 returned a response Scryer could not parse.",
        detail,
    )
}

fn get_version(config: &Aria2Config) -> Result<String, PluginError> {
    let xml = call_document(config, "aria2.getVersion", &[])?;
    let doc = parse_document(&xml)?;
    let value =
        first_response_value(&doc).ok_or_else(|| malformed("the version response was empty"))?;
    member_value(value, "version")
        .and_then(node_text)
        .ok_or_else(|| malformed("the version response carried no `version` member"))
}

fn get_globals(
    config: &Aria2Config,
) -> Result<std::collections::HashMap<String, String>, PluginError> {
    let xml = call_document(config, "aria2.getGlobalOption", &[])?;
    let doc = parse_document(&xml)?;
    let value = first_response_value(&doc)
        .ok_or_else(|| malformed("the global-option response was empty"))?;
    Ok(struct_members(value))
}

fn status_keys_param() -> String {
    xml_array(STATUS_KEYS.iter().copied().map(xml_string).collect())
}

fn list_torrents(config: &Aria2Config) -> Result<Vec<Aria2Status>, PluginError> {
    let mut out = Vec::new();
    for (method, args) in [
        ("aria2.tellActive", vec![status_keys_param()]),
        (
            "aria2.tellWaiting",
            vec![xml_int(0), xml_int(10 * 1024), status_keys_param()],
        ),
        (
            "aria2.tellStopped",
            vec![xml_int(0), xml_int(10 * 1024), status_keys_param()],
        ),
    ] {
        let xml = call_document(config, method, &args)?;
        out.extend(parse_status_array(&xml)?);
    }
    Ok(out)
}

fn tell_status(config: &Aria2Config, gid: &str) -> Result<Aria2Status, PluginError> {
    let xml = call_document(
        config,
        "aria2.tellStatus",
        &[xml_string(gid), status_keys_param()],
    )?;
    let doc = parse_document(&xml)?;
    let value =
        first_response_value(&doc).ok_or_else(|| malformed("the status response was empty"))?;
    Ok(parse_status(value))
}

/// `tellStatus` for a GID aria2 may no longer know. A "No such download"
/// answer is a fault, and every caller of this helper treats "cannot say" the
/// same as "not there", so the error is deliberately swallowed.
fn try_tell_status(config: &Aria2Config, gid: &str) -> Option<Aria2Status> {
    tell_status(config, gid)
        .ok()
        .filter(|status| !status.gid.is_empty())
}

// ---------------------------------------------------------------------------
// XML-RPC parsing
// ---------------------------------------------------------------------------

fn parse_status_array(xml: &str) -> Result<Vec<Aria2Status>, PluginError> {
    let doc = parse_document(xml)?;
    let value =
        first_response_value(&doc).ok_or_else(|| malformed("the listing response was empty"))?;
    Ok(array_values(value).into_iter().map(parse_status).collect())
}

fn parse_status(value: Node<'_, '_>) -> Aria2Status {
    let integer = |name: &str| {
        member_value(value, name)
            .and_then(node_text)
            .and_then(|value| value.trim().parse::<i64>().ok())
    };
    let bittorrent = member_value(value, "bittorrent");
    let info_hash = member_value(value, "infoHash").and_then(node_text);

    Aria2Status {
        // aria2 nests the torrent's display name at `bittorrent.info.name`
        // (`gatherBitTorrentMetadata`, `src/RpcMethodImpl.cc`), not at
        // `bittorrent.name`.
        bittorrent_name: bittorrent
            .and_then(|node| member_value(node, "info"))
            .and_then(|node| member_value(node, "name"))
            .and_then(node_text)
            .filter(|name| !name.trim().is_empty()),
        is_torrent: bittorrent.is_some() || info_hash.is_some(),
        info_hash,
        completed_length: integer("completedLength").unwrap_or_default(),
        download_speed: integer("downloadSpeed").unwrap_or_default(),
        upload_speed: integer("uploadSpeed").unwrap_or_default(),
        files: member_value(value, "files")
            .map(parse_files)
            .unwrap_or_default(),
        followed_by: member_value(value, "followedBy")
            .map(|node| {
                array_values(node)
                    .into_iter()
                    .filter_map(node_text)
                    .collect()
            })
            .unwrap_or_default(),
        gid: member_value(value, "gid")
            .and_then(node_text)
            .unwrap_or_default(),
        status: member_value(value, "status")
            .and_then(node_text)
            .unwrap_or_default(),
        total_length: integer("totalLength").unwrap_or_default(),
        upload_length: integer("uploadLength").unwrap_or_default(),
        // aria2 reports `errorCode` "0" for a download that never failed.
        error_code: member_value(value, "errorCode")
            .and_then(node_text)
            .filter(|code| code.trim() != "0" && !code.trim().is_empty()),
        error_message: member_value(value, "errorMessage")
            .and_then(node_text)
            .filter(|message| !message.trim().is_empty()),
        seeder: member_value(value, "seeder")
            .and_then(node_text)
            .and_then(|value| match value.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }),
        verified_length: integer("verifiedLength"),
    }
}

fn parse_files(value: Node<'_, '_>) -> Vec<String> {
    array_values(value)
        .into_iter()
        .filter_map(|file| member_value(file, "path").and_then(node_text))
        .collect()
}

fn first_response_value<'a>(doc: &'a Document<'a>) -> Option<Node<'a, 'a>> {
    doc.descendants()
        .find(|node| node.has_tag_name("param"))?
        .children()
        .find(|node| node.has_tag_name("value"))
}

/// The `<struct>` a value (or a `<fault>`) wraps.
fn struct_node<'a>(node: Node<'a, 'a>) -> Option<Node<'a, 'a>> {
    if node.has_tag_name("struct") {
        return Some(node);
    }
    if let Some(child) = node.children().find(|child| child.has_tag_name("struct")) {
        return Some(child);
    }
    node.children()
        .find(|child| child.has_tag_name("value"))?
        .children()
        .find(|child| child.has_tag_name("struct"))
}

/// One **direct** member of a struct.
///
/// This deliberately does not search descendants. aria2 emits its dictionaries
/// key-sorted, and a status struct nests `files[].uris[].status` — so a
/// descendant search for `status` finds a URI's `"used"`/`"waiting"` before the
/// download's own `active`/`complete`, and every non-BitTorrent download (and
/// every torrent-URL or magnet metadata fetch, which are the only rows that
/// carry `uris`) reports a fabricated state.
fn member_value<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    struct_node(node)?
        .children()
        .filter(|child| child.has_tag_name("member"))
        .find(|member| {
            member
                .children()
                .find(|child| child.has_tag_name("name"))
                .and_then(|child| child.text())
                == Some(name)
        })?
        .children()
        .find(|child| child.has_tag_name("value"))
}

/// The `<value>` children of the `<array><data>` a value wraps.
///
/// Scoped for the same reason as [`member_value`]: a status struct contains
/// several nested arrays (`files`, each file's `uris`, `followedBy`,
/// `bittorrent.announceList`), and a descendant search for `<data>` turns each
/// of them into phantom rows in the queue listing.
fn array_values<'a>(value: Node<'a, 'a>) -> Vec<Node<'a, 'a>> {
    let Some(data) = value
        .children()
        .find(|child| child.has_tag_name("array"))
        .and_then(|array| array.children().find(|child| child.has_tag_name("data")))
    else {
        return Vec::new();
    };
    data.children()
        .filter(|child| child.has_tag_name("value"))
        .collect()
}

fn struct_members(node: Node<'_, '_>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(structure) = struct_node(node) else {
        return out;
    };
    for member in structure
        .children()
        .filter(|child| child.has_tag_name("member"))
    {
        if let Some(name) = member
            .children()
            .find(|child| child.has_tag_name("name"))
            .and_then(|child| child.text())
            && let Some(value) = member
                .children()
                .find(|child| child.has_tag_name("value"))
                .and_then(node_text)
        {
            out.insert(name.to_string(), value);
        }
    }
    out
}

fn node_text(node: Node<'_, '_>) -> Option<String> {
    node.descendants()
        .find(|child| child.is_text() || child.text().is_some())
        .and_then(|child| child.text())
        .map(str::to_string)
}

fn check_fault(xml: &str) -> Result<(), PluginError> {
    if !xml.contains("<fault>") {
        return Ok(());
    }
    let doc = parse_document(xml)?;
    let fault = doc
        .descendants()
        .find(|node| node.has_tag_name("fault"))
        .ok_or_else(|| malformed("the fault element could not be read"))?;
    let code = member_value(fault, "faultCode")
        .and_then(node_text)
        .unwrap_or_default();
    let message = member_value(fault, "faultString")
        .and_then(node_text)
        .unwrap_or_default();
    Err(classify_fault(&code, &message))
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

fn looks_like_gid(value: &str) -> bool {
    value.len() == GID_HEX_LEN && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// The download the caller means, given an item id that is normally an info
/// hash and occasionally a bare GID.
fn resolve_gid(config: &Aria2Config, client_item_id: &str) -> Result<Option<String>, PluginError> {
    let requested = client_item_id.trim();
    if requested.is_empty() {
        return Ok(None);
    }

    // A GID answers for itself; the listing is three RPC calls and this is one.
    if looks_like_gid(requested)
        && let Some(status) = try_tell_status(config, requested)
    {
        return Ok(Some(payload_gid(config, status)));
    }

    let requested_hash = normalize_hash(requested);
    let mut metadata_match = None;
    for torrent in list_torrents(config)? {
        let matches = torrent.gid == requested
            || torrent
                .info_hash
                .as_deref()
                .map(normalize_hash)
                .is_some_and(|hash| !hash.is_empty() && hash == requested_hash);
        if !matches {
            continue;
        }
        // A magnet's `[METADATA]` placeholder carries the same info hash as the
        // payload download it is replaced by, so prefer the payload.
        if is_metadata_download(&torrent) {
            metadata_match.get_or_insert(torrent.gid);
        } else {
            return Ok(Some(torrent.gid));
        }
    }
    Ok(metadata_match)
}

/// Follow a `[METADATA]` download, or a `.torrent`-file fetch, to the payload
/// aria2 replaced it with.
fn payload_gid(config: &Aria2Config, status: Aria2Status) -> String {
    if (is_metadata_download(&status) || is_followed_fetch(&status))
        && let Some(followed) = status.followed_by.first()
        && let Some(payload) = try_tell_status(config, followed)
    {
        return payload.gid;
    }
    status.gid
}

/// The info hash aria2 already knows for a download that was just added.
///
/// There is no blocking sleep in the component runtime, so Sonarr's
/// 10×500 ms poll (`Aria2.cs:202-219`) cannot be ported. One opportunistic
/// lookup is enough for the case that matters: a magnet's metadata download
/// carries the `btih` from the URI as its `infoHash` from the moment it is
/// created, and a `.torrent` URL fetch names its payload in `followedBy`.
fn resolve_hash_after_add(config: &Aria2Config, gid: &str) -> Option<String> {
    let status = try_tell_status(config, gid)?;
    if let Some(hash) = status
        .info_hash
        .as_deref()
        .map(normalize_hash)
        .filter(|hash| hash.len() == 40)
    {
        return Some(hash);
    }
    let followed = status.followed_by.first()?;
    try_tell_status(config, followed)?
        .info_hash
        .as_deref()
        .map(normalize_hash)
        .filter(|hash| hash.len() == 40)
}

/// The release's hash first, then the magnet's `btih` (hex or base32), then
/// SHA-1 of the bencoded `info` dictionary — the same three sources Sonarr's
/// core resolves before it ever calls the client
/// (`TorrentClientBase.cs:208`, `:233`).
fn derive_info_hash(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request
        .release
        .info_hash_v1
        .clone()
        .or_else(|| request.release.info_hash_hint.clone())
        .map(|value| normalize_hash(&value))
        .filter(|value| value.len() == 40)
        .or_else(|| {
            [
                request.source.magnet_uri.as_deref(),
                request.source.download_url.as_deref(),
                request.source.torrent_url.as_deref(),
            ]
            .into_iter()
            .flatten()
            .find_map(parse_magnet_info_hash)
        })
        .or_else(|| {
            request
                .source
                .torrent_bytes_base64
                .as_deref()
                .and_then(|value| STANDARD.decode(value).ok())
                .and_then(|bytes| compute_torrent_info_hash(&bytes))
        })
}

fn parse_magnet_info_hash(uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if !trimmed
        .as_bytes()
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"magnet:?"))
    {
        return None;
    }
    for part in trimmed[8..].split('&') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("xt") && !key.eq_ignore_ascii_case("xt.1") {
            continue;
        }
        let decoded = percent_decode(value);
        if !decoded
            .as_bytes()
            .get(..9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"urn:btih:"))
        {
            continue;
        }
        if let Some(hash) = normalize_btih(&decoded[9..]) {
            return Some(hash);
        }
    }
    None
}

fn normalize_btih(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Some(value.to_ascii_lowercase());
    }
    if value.len() == 32 {
        let decoded = decode_base32(value)?;
        if decoded.len() == 20 {
            return Some(to_lower_hex(&decoded));
        }
    }
    None
}

/// RFC 4648 base32 without padding, the second `btih` encoding a magnet may
/// carry.
fn decode_base32(value: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut pending = 0u32;
    let mut out = Vec::with_capacity(value.len() * 5 / 8);
    for ch in value.chars() {
        if ch == '=' {
            break;
        }
        let index = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32,
            '2'..='7' => ch as u32 - '2' as u32 + 26,
            _ => return None,
        };
        bits = (bits << 5) | index;
        pending += 5;
        if pending >= 8 {
            pending -= 8;
            out.push((bits >> pending) as u8);
        }
    }
    Some(out)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'%'
            && idx + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[idx + 1..idx + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            idx += 3;
            continue;
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// SHA-1 over the raw bencoded `info` value, byte for byte as it appeared in
/// the file.
fn compute_torrent_info_hash(bytes: &[u8]) -> Option<String> {
    let (start, end) = find_info_dict_range(bytes)?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes[start..end]);
    Some(to_lower_hex(&hasher.finalize()))
}

fn find_info_dict_range(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.first().copied() != Some(b'd') {
        return None;
    }
    let mut idx = 1usize;
    while idx < bytes.len() && bytes[idx] != b'e' {
        let (key, value_start) = parse_bencoded_string(bytes, idx)?;
        let value_end = parse_bencoded_value(bytes, value_start)?;
        if key == b"info" {
            return Some((value_start, value_end));
        }
        idx = value_end;
    }
    None
}

fn parse_bencoded_string(bytes: &[u8], start: usize) -> Option<(&[u8], usize)> {
    let mut idx = start;
    while idx < bytes.len() && bytes[idx] != b':' {
        if !bytes[idx].is_ascii_digit() {
            return None;
        }
        idx += 1;
    }
    if idx >= bytes.len() || idx == start {
        return None;
    }
    let len = std::str::from_utf8(&bytes[start..idx])
        .ok()?
        .parse::<usize>()
        .ok()?;
    let data_start = idx + 1;
    let data_end = data_start.checked_add(len)?;
    if data_end > bytes.len() {
        return None;
    }
    Some((&bytes[data_start..data_end], data_end))
}

fn parse_bencoded_value(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start)? {
        b'i' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                idx += 1;
            }
            (idx < bytes.len()).then_some(idx + 1)
        }
        b'l' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                idx = parse_bencoded_value(bytes, idx)?;
            }
            (idx < bytes.len()).then_some(idx + 1)
        }
        b'd' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                let (_, next) = parse_bencoded_string(bytes, idx)?;
                idx = parse_bencoded_value(bytes, next)?;
            }
            (idx < bytes.len()).then_some(idx + 1)
        }
        b'0'..=b'9' => parse_bencoded_string(bytes, start).map(|(_, end)| end),
        _ => None,
    }
}

fn to_lower_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0f));
    }
    out
}

fn nibble_to_hex(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).unwrap_or('0')
}

// ---------------------------------------------------------------------------
// Item mapping
// ---------------------------------------------------------------------------

fn is_metadata_download(torrent: &Aria2Status) -> bool {
    torrent
        .files
        .first()
        .is_some_and(|path| path.contains("[METADATA]"))
}

/// A plain download aria2 has already followed to a BitTorrent payload: the
/// `.torrent` file fetched from a torrent URL (`--follow-torrent`). The payload
/// row carries the download; this row is only its metainfo.
fn is_followed_fetch(torrent: &Aria2Status) -> bool {
    !torrent.is_torrent && !torrent.followed_by.is_empty()
}

/// Sonarr skips the metadata download and anything already removed
/// (`Aria2.cs:82-88`). A `.torrent`-file fetch that aria2 has followed to its
/// payload is hidden for the same reason: Sonarr never creates one (its core
/// fetches the file itself), so it has no rule for it, but listing it would
/// offer a `.torrent` file for import.
fn is_visible_download(torrent: &Aria2Status) -> bool {
    !is_metadata_download(torrent) && !is_followed_fetch(torrent) && torrent.status != "removed"
}

/// The row title.
///
/// Sonarr uses `Bittorrent?.Name ?? ""` (`Aria2.cs:96`), which leaves a plain
/// URL download — something `aria2.addUri` accepts and Sonarr's own client can
/// produce from a torrent URL — as a blank row. aria2 has no name for one, so
/// fall back to the first file's basename.
fn download_title(torrent: &Aria2Status) -> String {
    if let Some(name) = torrent.bittorrent_name.clone() {
        return name;
    }
    torrent
        .files
        .first()
        .map(|path| {
            path.trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(path)
                .to_string()
        })
        .unwrap_or_default()
}

fn torrent_to_item(torrent: Aria2Status) -> PluginDownloadItem {
    let title = download_title(&torrent);
    let hash = torrent.info_hash.as_deref().map(normalize_hash);
    let id = hash.clone().unwrap_or_else(|| torrent.gid.clone());
    let remaining = (torrent.total_length - torrent.completed_length).max(0);
    let progress_percent = if torrent.total_length > 0 {
        Some(
            ((torrent.completed_length as f64 / torrent.total_length as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8,
        )
    } else {
        None
    };
    let eta = if torrent.download_speed > 0 {
        Some(remaining / torrent.download_speed)
    } else {
        None
    };
    let ratio = observed_ratio(&torrent);
    let can_remove = derive_can_remove(&torrent);
    let can_move_files = Some(is_data_complete(&torrent));
    let remote_output_path = get_output_path(&torrent);
    let message = status_message(&torrent);

    PluginDownloadItem {
        client_item_id: id.clone(),
        download_id: None,
        info_hash: hash.clone(),
        title,
        state: map_state(&torrent),
        message: message.clone(),
        category: None,
        remote_output_path: remote_output_path.clone(),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: hash,
            client_native_id: Some(torrent.gid.clone()),
            content_paths: remote_output_path.into_iter().collect(),
            uploaded_bytes: Some(torrent.upload_length),
            downloaded_bytes: Some(torrent.completed_length),
            download_rate_bytes_per_second: Some(torrent.download_speed),
            upload_rate_bytes_per_second: Some(torrent.upload_speed),
            seed_ratio: ratio,
            metadata_only: Some(false),
            is_encrypted: Some(false),
            raw_status: Some(torrent.status.clone()),
            status_reason: message,
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.total_length),
        remaining_size_bytes: Some(remaining),
        eta_seconds: eta,
        progress_percent,
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files,
        can_remove,
        removed: Some(torrent.status == "removed"),
        raw_state: Some(torrent.status),
        // aria2's status carries no completion timestamp — `tellStatus` has no
        // such member — so there is nothing honest to report here.
        completed_at: None,
    }
}

/// aria2 reports `errorCode` and `errorMessage` on a stopped download; Sonarr
/// surfaces only the message (`Aria2.cs:135`). Keep the message but name the
/// code when aria2 gave one without text.
fn status_message(torrent: &Aria2Status) -> Option<String> {
    match (
        torrent.error_message.as_deref(),
        torrent.error_code.as_deref(),
    ) {
        (Some(message), Some(code)) => Some(format!("{message} (aria2 error code {code})")),
        (Some(message), None) => Some(message.to_string()),
        (None, Some(code)) => Some(format!("aria2 error code {code}")),
        (None, None) => None,
    }
}

fn torrent_to_completed(torrent: Aria2Status) -> PluginCompletedDownload {
    let path = get_output_path(&torrent).unwrap_or_default();
    let hash = torrent.info_hash.as_deref().map(normalize_hash);
    // Sonarr's own output rule decides this: one file means the path *is* the
    // file, anything else means the longest common directory
    // (`Aria2.cs:260-268`). Extension sniffing was guessing at the same thing
    // and got a directory called `Show.S01.1080p` wrong.
    let output_kind = if torrent.files.len() == 1 {
        PluginDownloadOutputKind::File
    } else {
        PluginDownloadOutputKind::Directory
    };
    PluginCompletedDownload {
        client_item_id: hash.clone().unwrap_or_else(|| torrent.gid.clone()),
        download_id: None,
        info_hash: hash,
        name: download_title(&torrent),
        dest_dir: path.clone(),
        category: None,
        output_kind: Some(output_kind),
        content_paths: if path.is_empty() {
            Vec::new()
        } else {
            vec![path]
        },
        size_bytes: Some(torrent.total_length),
        completed_at: None,
        parameters: Vec::new(),
        release_name: torrent.bittorrent_name,
    }
}

fn get_output_path(torrent: &Aria2Status) -> Option<String> {
    longest_common_content_path(&torrent.files)
}

fn longest_common_content_path(paths: &[String]) -> Option<String> {
    let paths = paths
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    if paths.len() == 1 {
        return Some(paths[0].to_string());
    }

    let split_paths = paths
        .iter()
        .map(|path| path.split(['/', '\\']).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let max_common_len = split_paths
        .iter()
        .map(|parts| parts.len().saturating_sub(1))
        .min()
        .unwrap_or_default();
    let mut common_len = 0;
    for index in 0..max_common_len {
        let candidate = split_paths[0][index];
        if split_paths.iter().all(|parts| parts[index] == candidate) {
            common_len += 1;
        } else {
            break;
        }
    }

    if common_len == 0 {
        return None;
    }

    let separator = if paths[0].contains('\\') && !paths[0].contains('/') {
        "\\"
    } else {
        "/"
    };
    let mut common = split_paths[0][..common_len].join(separator);
    if common.is_empty() && (paths[0].starts_with('/') || paths[0].starts_with('\\')) {
        common = separator.to_string();
    }
    Some(common)
}

/// Whether the payload is fully downloaded and therefore movable.
///
/// A download aria2 is hash-checking is excluded: `verifiedLength` exists only
/// while the check is running (aria2 manual, `aria2.tellStatus`), and a failed
/// check sends aria2 back to downloading the pieces it rejected.
fn is_data_complete(torrent: &Aria2Status) -> bool {
    if torrent.verified_length.is_some() {
        return false;
    }
    torrent.status == "complete"
        || (torrent.total_length > 0 && torrent.completed_length >= torrent.total_length)
}

/// Honest `can_remove` for aria2.
///
/// aria2 keeps a BitTorrent download `active` while it is seeding and only moves it to
/// `complete` once seeding has stopped (`--seed-ratio` / `--seed-time` reached, or seeding
/// disabled). Those option values are not part of `tellStatus`, so the seeding goal of a
/// still-active torrent is unknowable here and Scryer-side evaluation decides.
fn derive_can_remove(torrent: &Aria2Status) -> Option<bool> {
    match torrent.status.as_str() {
        "complete" => Some(true),
        _ if is_data_complete(torrent) => None,
        _ => Some(false),
    }
}

/// Observed share ratio: uploaded over what has actually been downloaded so far.
///
/// Sonarr divides by `totalLength` (`Aria2.cs:139`); dividing by
/// `completedLength` is the ratio the tracker is actually counting while the
/// download is still in progress, and the two agree once it finishes.
fn observed_ratio(torrent: &Aria2Status) -> Option<f64> {
    (torrent.completed_length > 0)
        .then(|| torrent.upload_length as f64 / torrent.completed_length as f64)
}

/// A torrent aria2 is seeding: it says so itself with `seeder`, and a fully
/// downloaded torrent it still calls `active` is seeding by definition.
fn is_seeding(torrent: &Aria2Status) -> bool {
    torrent.is_torrent
        && (torrent.seeder == Some(true)
            || (torrent.total_length > 0 && torrent.completed_length >= torrent.total_length))
}

fn map_state(torrent: &Aria2Status) -> DownloadItemState {
    match torrent.status.as_str() {
        // `verifiedLength` exists only while a hash check is running.
        // Sonarr has no state for it and reports Downloading or Completed.
        "active" if torrent.verified_length.is_some() => DownloadItemState::Verifying,
        // Sonarr reports Completed here (`Aria2.cs:100-104`). aria2 keeps a
        // torrent `active` for exactly as long as it is seeding, and Scryer's
        // adapter maps `Seeding` onto the same completed queue state while
        // keeping the distinction visible
        // (`crates/scryer-plugins/src/download_client_adapter.rs:332`).
        "active" if is_seeding(torrent) => DownloadItemState::Seeding,
        "active" if is_data_complete(torrent) => DownloadItemState::Completed,
        "active" => DownloadItemState::Downloading,
        "waiting" => DownloadItemState::Queued,
        "paused" => DownloadItemState::Paused,
        "error" => DownloadItemState::Failed,
        "complete" => DownloadItemState::Completed,
        "removed" => DownloadItemState::Completed,
        // aria2 only ever reports the six statuses above, so this arm is for a
        // future aria2 that grows one. An unrecognised status is not evidence
        // of a failure, and a Warning here would be a queue row nothing ever
        // clears; keep polling instead. (Sonarr leaves its pre-initialised
        // `DownloadItemStatus.Failed` in place for the same case,
        // Download/Clients/Aria2/Aria2.cs:95 — the harsher choice, and the one
        // Scryer's failed-download cleanup makes destructive.)
        _ => DownloadItemState::Downloading,
    }
}

fn source_url(request: &PluginDownloadClientAddRequest) -> Option<String> {
    match request.source.kind {
        DownloadInputKind::MagnetUri => request
            .source
            .magnet_uri
            .clone()
            .or_else(|| request.source.download_url.clone()),
        DownloadInputKind::TorrentUrl
        | DownloadInputKind::TorrentFile
        | DownloadInputKind::TorrentBytes => request
            .source
            .torrent_url
            .clone()
            .or_else(|| request.source.download_url.clone())
            .or_else(|| request.source.magnet_uri.clone()),
        DownloadInputKind::Nzb | DownloadInputKind::NzbUrl => None,
    }
}

// ---------------------------------------------------------------------------
// XML-RPC encoding
// ---------------------------------------------------------------------------

fn xml_string(value: &str) -> String {
    format!("<string>{}</string>", xml_escape(value))
}

fn xml_int(value: i64) -> String {
    format!("<int>{value}</int>")
}

fn xml_base64(bytes: &[u8]) -> String {
    format!("<base64>{}</base64>", STANDARD.encode(bytes))
}

fn xml_array(values: Vec<String>) -> String {
    format!(
        "<array><data>{}</data></array>",
        values
            .into_iter()
            .map(|value| format!("<value>{value}</value>"))
            .collect::<Vec<_>>()
            .join("")
    )
}

fn xml_struct(values: &[(String, String)]) -> String {
    format!(
        "<struct>{}</struct>",
        values
            .iter()
            .map(|(key, value)| format!(
                "<member><name>{}</name><value>{}</value></member>",
                xml_escape(key),
                xml_string(value)
            ))
            .collect::<Vec<_>>()
            .join("")
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn normalize_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// `left < right`, with a version aria2 reports in a shape this cannot read
/// treated as "not older".
///
/// Sonarr's `new Version(version)` throws on such a string and its catch turns
/// the whole test into "unable to connect" (`Aria2.cs:237`, `:247`); refusing
/// to guess is the better half of that behaviour.
fn version_is_older_than(left: &str, right: &str) -> bool {
    let Some(left) = parse_version(left) else {
        return false;
    };
    let Some(right) = parse_version(right) else {
        return false;
    };
    for index in 0..left.len().max(right.len()) {
        let l = left.get(index).copied().unwrap_or_default();
        let r = right.get(index).copied().unwrap_or_default();
        if l != r {
            return l < r;
        }
    }
    false
}

fn parse_version(value: &str) -> Option<Vec<u32>> {
    let parts = value
        .trim()
        .split('.')
        .map(|part| part.trim().parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then_some(parts)
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn config_bool(key: &str, default: bool) -> bool {
    config_value(key)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
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

fn connection_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        role: Some(ConfigFieldRole::ConnectionUrl),
        ..field(
            key,
            label,
            ConfigFieldType::String,
            required,
            default_value,
            help_text,
        )
    }
}

fn is_localhost_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1") || lower.contains("://[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::PluginTorrentOptions;

    fn status(status: &str, completed: i64) -> Aria2Status {
        Aria2Status {
            bittorrent_name: Some("Movie".to_string()),
            info_hash: Some("abcdef0123456789abcdef0123456789abcdef01".to_string()),
            completed_length: completed,
            download_speed: 0,
            upload_speed: 0,
            files: vec!["/downloads/Movie/Movie.mkv".to_string()],
            followed_by: Vec::new(),
            gid: "2089b05ecca3d829".to_string(),
            status: status.to_string(),
            total_length: 1_000,
            upload_length: 2_000,
            error_code: None,
            error_message: None,
            is_torrent: true,
            seeder: None,
            verified_length: None,
        }
    }

    // -----------------------------------------------------------------------
    // Seeding audit (unchanged behaviour, kept pinned)
    // -----------------------------------------------------------------------

    #[test]
    fn can_remove_is_false_while_downloading() {
        let torrent = status("active", 400);
        assert_eq!(derive_can_remove(&torrent), Some(false));
        assert!(!is_data_complete(&torrent));
    }

    #[test]
    fn can_remove_is_unknown_while_aria2_is_still_seeding() {
        // aria2 leaves a fully downloaded torrent `active` while it seeds, and the
        // seed-ratio/seed-time options are not part of tellStatus.
        let torrent = status("active", 1_000);
        assert_eq!(derive_can_remove(&torrent), None);
    }

    #[test]
    fn can_remove_is_true_once_aria2_reports_complete() {
        assert_eq!(derive_can_remove(&status("complete", 1_000)), Some(true));
    }

    #[test]
    fn can_remove_is_unknown_for_a_paused_complete_download() {
        assert_eq!(derive_can_remove(&status("paused", 1_000)), None);
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let item = torrent_to_item(status("active", 1_000));
        assert_eq!(item.can_move_files, Some(true));
        assert_eq!(item.can_remove, None);

        let downloading = torrent_to_item(status("active", 400));
        assert_eq!(downloading.can_move_files, Some(false));
    }

    #[test]
    fn observed_ratio_uses_completed_length() {
        assert_eq!(observed_ratio(&status("active", 1_000)), Some(2.0));
        assert_eq!(observed_ratio(&status("active", 0)), None);
    }

    #[test]
    fn is_private_is_never_claimed_because_aria2_does_not_report_it() {
        let item = torrent_to_item(status("complete", 1_000));
        assert_eq!(item.torrent.unwrap().is_private, None);
    }

    // -----------------------------------------------------------------------
    // Status table (Aria2.cs:98-123)
    // -----------------------------------------------------------------------

    #[test]
    fn an_unrecognised_status_keeps_polling_instead_of_warning_or_failing() {
        // A status aria2 does not document today must not park the row in a
        // state nothing clears, nor trip Scryer's failed-download cleanup.
        assert_eq!(
            map_state(&status("somethingNew", 400)),
            DownloadItemState::Downloading
        );
        // The documented statuses keep their meaning.
        assert_eq!(map_state(&status("error", 400)), DownloadItemState::Failed);
        assert_eq!(map_state(&status("paused", 400)), DownloadItemState::Paused);
        assert_eq!(
            map_state(&status("complete", 1_000)),
            DownloadItemState::Completed
        );
    }

    #[test]
    fn the_full_status_table_matches_sonarr_except_where_scryer_is_more_precise() {
        assert_eq!(
            map_state(&status("active", 400)),
            DownloadItemState::Downloading
        );
        assert_eq!(map_state(&status("waiting", 0)), DownloadItemState::Queued);
        assert_eq!(
            map_state(&status("removed", 0)),
            DownloadItemState::Completed
        );
        // Sonarr says Completed for an `active` torrent whose data is done; it
        // is seeding, and Scryer can say so.
        assert_eq!(
            map_state(&status("active", 1_000)),
            DownloadItemState::Seeding
        );
    }

    #[test]
    fn aria2s_own_seeder_flag_reports_seeding_before_the_lengths_agree() {
        // A torrent aria2 calls a seeder is seeding even if `--select-file`
        // left `completedLength` short of `totalLength`.
        let mut torrent = status("active", 400);
        torrent.seeder = Some(true);
        assert_eq!(map_state(&torrent), DownloadItemState::Seeding);
    }

    #[test]
    fn a_finished_plain_url_download_is_completed_not_seeding() {
        let mut torrent = status("active", 1_000);
        torrent.is_torrent = false;
        torrent.info_hash = None;
        torrent.bittorrent_name = None;
        assert_eq!(map_state(&torrent), DownloadItemState::Completed);
    }

    #[test]
    fn a_hash_check_in_progress_is_verifying_and_not_movable() {
        let mut torrent = status("active", 1_000);
        torrent.verified_length = Some(500);
        assert_eq!(map_state(&torrent), DownloadItemState::Verifying);
        assert!(!is_data_complete(&torrent));
        assert_eq!(torrent_to_item(torrent).can_move_files, Some(false));
    }

    // -----------------------------------------------------------------------
    // Output path (Aria2.cs:260-268)
    // -----------------------------------------------------------------------

    #[test]
    fn a_single_file_download_reports_that_file() {
        let torrent = status("complete", 1_000);
        assert_eq!(
            get_output_path(&torrent).as_deref(),
            Some("/downloads/Movie/Movie.mkv")
        );
        let completed = torrent_to_completed(torrent);
        assert_eq!(completed.output_kind, Some(PluginDownloadOutputKind::File));
    }

    #[test]
    fn a_multi_file_download_reports_the_longest_common_directory() {
        let mut torrent = status("complete", 1_000);
        torrent.files = vec![
            "/downloads/Show.S01/Show.S01E01.mkv".to_string(),
            "/downloads/Show.S01/Show.S01E02.mkv".to_string(),
            "/downloads/Show.S01/extras/behind.mkv".to_string(),
        ];
        assert_eq!(
            get_output_path(&torrent).as_deref(),
            Some("/downloads/Show.S01")
        );
        let completed = torrent_to_completed(torrent);
        assert_eq!(
            completed.output_kind,
            Some(PluginDownloadOutputKind::Directory)
        );
        // A directory without a dot used to be classified as a file by the
        // extension sniffing this replaced; one without an extension is now
        // classified by the file count, exactly like Sonarr's rule.
        assert_eq!(completed.dest_dir, "/downloads/Show.S01");
    }

    #[test]
    fn a_dotted_directory_name_is_still_a_directory() {
        let mut torrent = status("complete", 1_000);
        torrent.files = vec![
            "/downloads/Show.S01.1080p.WEB/a.mkv".to_string(),
            "/downloads/Show.S01.1080p.WEB/b.mkv".to_string(),
        ];
        assert_eq!(
            torrent_to_completed(torrent).output_kind,
            Some(PluginDownloadOutputKind::Directory)
        );
    }

    #[test]
    fn files_with_no_shared_directory_report_no_output_path() {
        assert_eq!(
            longest_common_content_path(&["/a/one.mkv".to_string(), "/b/two.mkv".to_string()]),
            Some("/".to_string())
        );
        assert_eq!(
            longest_common_content_path(&["a/one.mkv".to_string(), "b/two.mkv".to_string()]),
            None
        );
        assert_eq!(longest_common_content_path(&[]), None);
    }

    // -----------------------------------------------------------------------
    // Titles (Aria2.cs:96)
    // -----------------------------------------------------------------------

    #[test]
    fn a_non_bittorrent_download_is_titled_by_its_file_rather_than_left_blank() {
        let mut torrent = status("complete", 1_000);
        torrent.bittorrent_name = None;
        torrent.is_torrent = false;
        torrent.files = vec!["/downloads/Show.S01E01.1080p.mkv".to_string()];
        assert_eq!(download_title(&torrent), "Show.S01E01.1080p.mkv");
        assert_eq!(torrent_to_item(torrent).title, "Show.S01E01.1080p.mkv");
    }

    #[test]
    fn a_torrent_keeps_its_metainfo_name() {
        assert_eq!(download_title(&status("active", 400)), "Movie");
    }

    // -----------------------------------------------------------------------
    // XML-RPC parsing
    // -----------------------------------------------------------------------

    /// One `tellStopped` answer shaped the way aria2 actually emits it: keys in
    /// sorted order, `files` before `status`, and each file carrying a `uris`
    /// array whose entries have a `status` member of their own.
    fn tell_stopped_response() -> String {
        r#"<?xml version="1.0"?>
<methodResponse><params><param><value><array><data>
  <value><struct>
    <member><name>bittorrent</name><value><struct>
      <member><name>info</name><value><struct>
        <member><name>name</name><value><string>Show.S01</string></value></member>
      </struct></value></member>
      <member><name>mode</name><value><string>multi</string></value></member>
    </struct></value></member>
    <member><name>completedLength</name><value><string>2000</string></value></member>
    <member><name>downloadSpeed</name><value><string>0</string></value></member>
    <member><name>errorCode</name><value><string>0</string></value></member>
    <member><name>files</name><value><array><data>
      <value><struct>
        <member><name>index</name><value><string>1</string></value></member>
        <member><name>path</name><value><string>/downloads/Show.S01/a.mkv</string></value></member>
        <member><name>uris</name><value><array><data>
          <value><struct>
            <member><name>status</name><value><string>used</string></value></member>
            <member><name>uri</name><value><string>http://example.invalid/a</string></value></member>
          </struct></value>
        </data></array></value></member>
      </struct></value>
      <value><struct>
        <member><name>index</name><value><string>2</string></value></member>
        <member><name>path</name><value><string>/downloads/Show.S01/b.mkv</string></value></member>
        <member><name>uris</name><value><array><data></data></array></value></member>
      </struct></value>
    </data></array></value></member>
    <member><name>followedBy</name><value><array><data>
      <value><string>1111111111111111</string></value>
    </data></array></value></member>
    <member><name>gid</name><value><string>2089b05ecca3d829</string></value></member>
    <member><name>infoHash</name><value><string>ABCDEF0123456789ABCDEF0123456789ABCDEF01</string></value></member>
    <member><name>status</name><value><string>complete</string></value></member>
    <member><name>totalLength</name><value><string>2000</string></value></member>
    <member><name>uploadLength</name><value><string>4000</string></value></member>
    <member><name>uploadSpeed</name><value><string>17</string></value></member>
  </struct></value>
</data></array></value></param></params></methodResponse>"#
            .to_string()
    }

    #[test]
    fn a_listing_yields_one_row_per_download_and_not_one_per_nested_array() {
        // A descendant search for `<data>` turned every `files`, `uris` and
        // `followedBy` array into extra rows with an empty client_item_id.
        let statuses = parse_status_array(&tell_stopped_response()).expect("parse the listing");
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].gid, "2089b05ecca3d829");
    }

    #[test]
    fn the_downloads_own_status_is_read_rather_than_a_uris_status() {
        // aria2 emits its dictionaries key-sorted, so `files` precedes
        // `status`: a descendant search finds a URI's "used" first.
        let statuses = parse_status_array(&tell_stopped_response()).expect("parse the listing");
        assert_eq!(statuses[0].status, "complete");
        assert_eq!(map_state(&statuses[0]), DownloadItemState::Completed);
    }

    #[test]
    fn a_status_parses_every_member_this_plugin_relies_on() {
        let statuses = parse_status_array(&tell_stopped_response()).expect("parse the listing");
        let parsed = &statuses[0];
        assert_eq!(parsed.bittorrent_name.as_deref(), Some("Show.S01"));
        assert!(parsed.is_torrent);
        assert_eq!(parsed.total_length, 2_000);
        assert_eq!(parsed.completed_length, 2_000);
        assert_eq!(parsed.upload_length, 4_000);
        assert_eq!(parsed.upload_speed, 17);
        assert_eq!(
            parsed.files,
            vec![
                "/downloads/Show.S01/a.mkv".to_string(),
                "/downloads/Show.S01/b.mkv".to_string()
            ]
        );
        assert_eq!(parsed.followed_by, vec!["1111111111111111".to_string()]);
        // `errorCode` "0" means "no error" and must not become a message.
        assert_eq!(parsed.error_code, None);
        assert_eq!(status_message(parsed), None);
    }

    #[test]
    fn the_item_id_is_the_lowercased_info_hash_and_the_gid_is_kept_as_the_native_id() {
        let statuses = parse_status_array(&tell_stopped_response()).expect("parse the listing");
        let item = torrent_to_item(statuses.into_iter().next().expect("one row"));
        assert_eq!(
            item.client_item_id,
            "abcdef0123456789abcdef0123456789abcdef01"
        );
        assert_eq!(
            item.torrent
                .expect("torrent block")
                .client_native_id
                .as_deref(),
            Some("2089b05ecca3d829")
        );
    }

    #[test]
    fn an_error_code_without_a_message_is_still_surfaced() {
        let mut torrent = status("error", 400);
        torrent.error_code = Some("24".to_string());
        assert_eq!(
            status_message(&torrent).as_deref(),
            Some("aria2 error code 24")
        );
        torrent.error_message = Some("HTTP authorization failed".to_string());
        assert_eq!(
            status_message(&torrent).as_deref(),
            Some("HTTP authorization failed (aria2 error code 24)")
        );
    }

    #[test]
    fn a_metadata_download_is_hidden_the_way_sonarr_hides_it() {
        let mut torrent = status("active", 0);
        torrent.files = vec!["/downloads/[METADATA]Show.S01".to_string()];
        assert!(!is_visible_download(&torrent));
        assert!(is_metadata_download(&torrent));

        let mut removed = status("complete", 1_000);
        removed.status = "removed".to_string();
        assert!(!is_visible_download(&removed));
    }

    // -----------------------------------------------------------------------
    // Faults and HTTP classification (00-common.md rule 4)
    // -----------------------------------------------------------------------

    #[test]
    fn a_rejected_secret_token_is_an_auth_failure_not_a_temporary_one() {
        // aria2 answers every method failure with faultCode 1
        // (`RpcMethod::createErrorResponse`), so the fault string is the only
        // discriminator; a bad `--rpc-secret` is `DL_ABORT_EX("Unauthorized")`.
        let fault = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
            <member><name>faultCode</name><value><int>1</int></value></member>
            <member><name>faultString</name><value><string>Unauthorized</string></value></member>
        </struct></value></fault></methodResponse>"#;
        let error = check_fault(fault).expect_err("a fault must not be Ok");
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert_eq!(
            error.debug_message.as_deref(),
            Some("Aria2 returned error code 1: Unauthorized")
        );
    }

    #[test]
    fn any_other_fault_is_permanent_and_keeps_sonarrs_wording_in_the_debug_message() {
        let fault = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
            <member><name>faultCode</name><value><int>1</int></value></member>
            <member><name>faultString</name><value><string>No such download for GID#2089b05ecca3d829</string></value></member>
        </struct></value></fault></methodResponse>"#;
        let error = check_fault(fault).expect_err("a fault must not be Ok");
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(
            error
                .debug_message
                .as_deref()
                .expect("debug detail")
                .starts_with("Aria2 returned error code 1: No such download")
        );
    }

    #[test]
    fn a_successful_response_is_not_mistaken_for_a_fault() {
        assert!(check_fault(&tell_stopped_response()).is_ok());
    }

    #[test]
    fn http_statuses_map_to_the_codes_the_operator_can_act_on() {
        assert!(classify_http_status(200, None, "").is_none());
        assert_eq!(
            classify_http_status(302, Some("/login"), "")
                .expect("a redirect is a configuration problem")
                .code,
            PluginErrorCode::InvalidConfig
        );
        assert_eq!(
            classify_http_status(401, None, "").expect("401").code,
            PluginErrorCode::AuthFailed
        );
        assert_eq!(
            classify_http_status(404, None, "").expect("404").code,
            PluginErrorCode::InvalidConfig
        );
        assert_eq!(
            classify_http_status(503, None, "busy").expect("503").code,
            PluginErrorCode::Temporary
        );
        assert_eq!(
            classify_http_status(418, None, "").expect("418").code,
            PluginErrorCode::Permanent
        );
    }

    #[test]
    fn transport_failures_separate_timeouts_from_unreachable_hosts() {
        assert_eq!(
            classify_transport_error("operation timed out").code,
            PluginErrorCode::Temporary
        );
        assert_eq!(
            classify_transport_error("certificate verify failed").code,
            PluginErrorCode::UpstreamUnavailable
        );
        assert_eq!(
            classify_transport_error("connection refused").code,
            PluginErrorCode::UpstreamUnavailable
        );
    }

    // -----------------------------------------------------------------------
    // Version gate (Aria2.cs:237-244)
    // -----------------------------------------------------------------------

    #[test]
    fn the_version_gate_matches_sonarrs_1_34_0_floor() {
        assert!(version_is_older_than("1.33.1", MIN_SUPPORTED_VERSION));
        assert!(version_is_older_than("1.19.0", MIN_SUPPORTED_VERSION));
        assert!(!version_is_older_than("1.34.0", MIN_SUPPORTED_VERSION));
        assert!(!version_is_older_than("1.36.0", MIN_SUPPORTED_VERSION));
        assert!(!version_is_older_than("1.37.0", MIN_SUPPORTED_VERSION));
    }

    #[test]
    fn a_version_string_that_cannot_be_read_is_not_treated_as_too_old() {
        // Sonarr's `new Version(..)` throws here and the whole test collapses
        // into "unable to connect"; refusing to guess is the better half.
        assert!(!version_is_older_than("1.37.0-dev", MIN_SUPPORTED_VERSION));
        assert!(!version_is_older_than("", MIN_SUPPORTED_VERSION));
    }

    // -----------------------------------------------------------------------
    // Add options
    // -----------------------------------------------------------------------

    fn add_request(kind: &str) -> PluginDownloadClientAddRequest {
        serde_json::from_str(&format!(
            r#"{{"source":{{"kind":"{kind}"}},"release":{{}},
                "title":{{"title_name":"Show","media_facet":"tv"}},"routing":{{}}}}"#
        ))
        .expect("a minimal add request")
    }

    fn config() -> Aria2Config {
        Aria2Config {
            rpc_url: "http://localhost:6800/rpc".to_string(),
            secret_token: "MySecretToken".to_string(),
            directory: "/downloads".to_string(),
        }
    }

    #[test]
    fn the_routed_directory_wins_over_the_configured_one() {
        let mut request = add_request("magnet_uri");
        request.routing.download_directory = Some("/downloads/tv".to_string());
        assert_eq!(
            add_options(&config(), &request),
            vec![("dir".to_string(), "/downloads/tv".to_string())]
        );

        request.routing.download_directory = Some("   ".to_string());
        assert_eq!(
            add_options(&config(), &request),
            vec![("dir".to_string(), "/downloads".to_string())]
        );
    }

    #[test]
    fn no_directory_at_all_sends_no_dir_option() {
        let mut config = config();
        config.directory = String::new();
        assert!(add_options(&config, &add_request("magnet_uri")).is_empty());
    }

    #[test]
    fn a_seeding_goal_becomes_arias_own_per_download_seed_options() {
        // `seed-ratio` is a share ratio and `seed-time` is in minutes
        // (aria2 manual, "Input File" / `--seed-time`). Sonarr sets neither.
        let mut request = add_request("magnet_uri");
        request.torrent = Some(PluginTorrentOptions {
            seed_goal_ratio: Some(1.5),
            seed_goal_seconds: Some(3_600),
            ..PluginTorrentOptions::default()
        });
        let options = add_options(&config(), &request);
        assert!(options.contains(&("seed-ratio".to_string(), "1.5".to_string())));
        assert!(options.contains(&("seed-time".to_string(), "60".to_string())));
    }

    #[test]
    fn a_release_level_seeding_goal_is_used_when_the_torrent_block_is_absent() {
        let mut request = add_request("magnet_uri");
        request.release.seed_goal_ratio = Some(2.0);
        request.release.seed_goal_seconds = Some(90);
        let options = add_options(&config(), &request);
        assert!(options.contains(&("seed-ratio".to_string(), "2".to_string())));
        assert!(options.contains(&("seed-time".to_string(), "1.5".to_string())));
    }

    #[test]
    fn a_zero_ratio_goal_disables_seeding_instead_of_seeding_forever() {
        // aria2's `seed-ratio=0.0` means "seed regardless of ratio"; Scryer's
        // gate means "obligation met at once". `seed-time=0` is aria2's way to
        // say the latter, and it wins over any time goal (OR-semantics).
        let mut request = add_request("magnet_uri");
        request.torrent = Some(PluginTorrentOptions {
            seed_goal_ratio: Some(0.0),
            seed_goal_seconds: Some(3_600),
            ..PluginTorrentOptions::default()
        });
        let options = add_options(&config(), &request);
        assert!(options.contains(&("seed-time".to_string(), "0".to_string())));
        assert!(!options.iter().any(|(key, _)| key == "seed-ratio"));
    }

    #[test]
    fn a_followed_torrent_file_fetch_is_hidden_once_its_payload_exists() {
        let mut fetch = status("complete", 1_000);
        fetch.is_torrent = false;
        fetch.info_hash = None;
        fetch.bittorrent_name = None;
        fetch.files = vec!["/downloads/Show.S01.torrent".to_string()];
        assert!(is_visible_download(&fetch));
        fetch.followed_by = vec!["1111111111111111".to_string()];
        assert!(!is_visible_download(&fetch));
        assert!(is_followed_fetch(&fetch));
    }

    #[test]
    fn a_missing_or_nonsensical_seeding_goal_sends_nothing() {
        let mut request = add_request("magnet_uri");
        request.torrent = Some(PluginTorrentOptions {
            seed_goal_ratio: Some(f64::NAN),
            seed_goal_seconds: Some(0),
            ..PluginTorrentOptions::default()
        });
        assert_eq!(
            add_options(&config(), &request),
            vec![("dir".to_string(), "/downloads".to_string())]
        );
    }

    // -----------------------------------------------------------------------
    // Info-hash identity (H1)
    // -----------------------------------------------------------------------

    #[test]
    fn a_magnets_btih_is_the_item_identity_even_without_a_release_hash() {
        let mut request = add_request("magnet_uri");
        request.source.magnet_uri = Some(
            "magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789ABCDEF01&dn=Show".to_string(),
        );
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
    }

    #[test]
    fn a_base32_btih_is_decoded_to_hex() {
        // 32 base32 characters decode to the same 20 bytes as the hex form.
        let mut request = add_request("magnet_uri");
        request.source.magnet_uri =
            Some("magnet:?xt=urn%3Abtih%3AVXY3YARJRPTZBIMPSVPBJPBMFCXKUKQR".to_string());
        let hash = derive_info_hash(&request).expect("a base32 btih is a hash");
        assert_eq!(hash.len(), 40);
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn torrent_bytes_are_hashed_over_the_bencoded_info_dictionary() {
        // d8:announce3:foo4:infod4:name3:bar6:lengthi1eee
        let torrent = b"d8:announce3:foo4:infod4:name3:bar6:lengthi1eee";
        let expected = {
            let mut hasher = Sha1::new();
            hasher.update(b"d4:name3:bar6:lengthi1ee");
            to_lower_hex(&hasher.finalize())
        };
        let mut request = add_request("torrent_bytes");
        request.source.torrent_bytes_base64 = Some(STANDARD.encode(torrent));
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn the_release_hash_still_wins_when_scryer_supplies_one() {
        let mut request = add_request("magnet_uri");
        request.release.info_hash_v1 = Some("ABCDEF0123456789ABCDEF0123456789ABCDEF01".to_string());
        request.source.magnet_uri =
            Some("magnet:?xt=urn:btih:1111111111111111111111111111111111111111".to_string());
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
    }

    #[test]
    fn a_source_with_no_hash_anywhere_derives_none() {
        let mut request = add_request("torrent_url");
        request.source.torrent_url = Some("http://example.invalid/file.torrent".to_string());
        assert_eq!(derive_info_hash(&request), None);
    }

    #[test]
    fn a_gid_is_told_apart_from_an_info_hash_by_length() {
        assert!(looks_like_gid("2089b05ecca3d829"));
        assert!(!looks_like_gid("abcdef0123456789abcdef0123456789abcdef01"));
        assert!(!looks_like_gid("2089b05ecca3d82z"));
        assert!(!looks_like_gid(""));
    }

    // -----------------------------------------------------------------------
    // Descriptor contract
    // -----------------------------------------------------------------------

    fn descriptor() -> serde_json::Value {
        serde_json::from_str(&scryer_describe(String::new()).expect("describe"))
            .expect("a descriptor")
    }

    #[test]
    fn post_import_configuration_is_retired_and_the_handoff_is_declared_absent() {
        let value = descriptor();
        let keys = value["provider"]["config_fields"]
            .as_array()
            .expect("config fields")
            .iter()
            .map(|field| field["key"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert!(
            !keys.iter().any(|key| key == "post_import_action"),
            "the retired option must no longer be offered: {keys:?}"
        );
        // aria2 has no label, tag or category to hand off to, so both marks are
        // declared absent and the core skips the handoff entirely.
        assert_eq!(value["provider"]["capabilities"]["mark_imported"], false);
        assert_eq!(
            value["provider"]["capabilities"]["mark_imported_non_destructive"],
            false
        );
    }

    #[test]
    fn a_legacy_post_import_action_value_is_inert() {
        // Both stored values are no-ops now: nothing reads the key, and the
        // destructive mark is an acknowledged no-op rather than a removal.
        let request = r#"{"client_item_id":"abcdef0123456789abcdef0123456789abcdef01"}"#;
        let raw = scryer_download_mark_imported(request.to_string()).expect("mark imported");
        let result: PluginResult<()> = serde_json::from_str(&raw).expect("a plugin result");
        assert!(matches!(result, PluginResult::Ok(())));
    }

    #[test]
    fn the_descriptor_reports_the_seed_limits_aria2_can_actually_carry() {
        let value = descriptor();
        assert_eq!(value["provider"]["capabilities"]["seed_limits"], true);
        let torrent = &value["provider"]["capabilities"]["torrent"];
        assert_eq!(torrent["supports_seed_ratio_limit"], true);
        assert_eq!(torrent["supports_seed_time_limit"], true);
        // aria2 stops seeding when a limit is met; it never drops the entry.
        assert_eq!(
            torrent["removes_on_seed_limit"],
            serde_json::Value::Bool(false)
        );
    }

    #[test]
    fn remove_with_data_is_declared_false_because_aria2_cannot_delete_files() {
        assert_eq!(
            descriptor()["provider"]["capabilities"]["remove_with_data"],
            false
        );
    }

    #[test]
    fn the_user_agent_carries_the_crate_version() {
        assert_eq!(
            user_agent(),
            format!("scryer-aria2-plugin/{}", env!("CARGO_PKG_VERSION"))
        );
        assert!(!user_agent().ends_with("/0.1"));
    }

    // -----------------------------------------------------------------------
    // Listing shape
    // -----------------------------------------------------------------------

    #[test]
    fn history_is_empty_because_the_queue_listing_already_carries_aria2s_errors() {
        let raw = scryer_download_list_history(String::new()).expect("list history");
        let result: PluginResult<Vec<PluginDownloadItem>> =
            serde_json::from_str(&raw).expect("a plugin result");
        match result {
            PluginResult::Ok(items) => assert!(items.is_empty()),
            PluginResult::Err(error) => panic!("history must not fail: {error:?}"),
        }
    }

    #[test]
    fn the_status_key_set_asks_for_every_member_the_parser_reads() {
        for key in [
            "gid",
            "status",
            "totalLength",
            "completedLength",
            "uploadLength",
            "downloadSpeed",
            "uploadSpeed",
            "infoHash",
            "seeder",
            "errorCode",
            "errorMessage",
            "followedBy",
            "files",
            "bittorrent",
            "verifiedLength",
        ] {
            assert!(
                STATUS_KEYS.contains(&key),
                "{key} is parsed but not requested"
            );
        }
        // `bitfield` is the one large member aria2 would otherwise send on
        // every poll of every torrent.
        assert!(!STATUS_KEYS.contains(&"bitfield"));
    }

    #[test]
    fn the_keys_argument_is_encoded_as_an_xml_rpc_string_array() {
        let encoded = status_keys_param();
        assert!(encoded.starts_with("<array><data>"));
        assert!(encoded.contains("<value><string>status</string></value>"));
        assert!(encoded.ends_with("</data></array>"));
    }

    // -----------------------------------------------------------------------
    // Control (Aria2.cs:152-189)
    // -----------------------------------------------------------------------

    #[test]
    fn removal_picks_the_call_sonarr_picks_for_each_status() {
        for terminal in ["complete", "error", "removed"] {
            assert_eq!(
                removal_call(Some(terminal)),
                ("aria2.removeDownloadResult", true),
                "{terminal} is a stopped result"
            );
        }
        for running in ["active", "waiting", "paused"] {
            assert_eq!(
                removal_call(Some(running)),
                ("aria2.forceRemove", false),
                "{running} is a live download"
            );
        }
        assert_eq!(removal_call(None), ("aria2.forceRemove", false));
    }

    #[test]
    fn force_start_is_refused_before_the_daemon_is_contacted() {
        // aria2 has no force-start; the answer must be `Unsupported`, not the
        // `Temporary` every bare `Error::msg` used to become.
        let raw = scryer_download_control(
            r#"{"action":"force_start","client_item_id":"2089b05ecca3d829"}"#.to_string(),
        )
        .expect("control answers");
        let result: PluginResult<()> = serde_json::from_str(&raw).expect("a plugin result");
        match result {
            PluginResult::Err(error) => assert_eq!(error.code, PluginErrorCode::Unsupported),
            PluginResult::Ok(()) => panic!("force_start must not report success"),
        }
    }

    #[test]
    fn a_malformed_control_request_is_permanent_not_temporary() {
        let raw = scryer_download_control("{".to_string()).expect("control answers");
        let result: PluginResult<()> = serde_json::from_str(&raw).expect("a plugin result");
        match result {
            PluginResult::Err(error) => assert_eq!(error.code, PluginErrorCode::Permanent),
            PluginResult::Ok(()) => panic!("a malformed request must not report success"),
        }
    }

    #[test]
    fn pause_and_resume_are_advertised_because_aria2_has_both_calls() {
        let value = descriptor();
        assert_eq!(value["provider"]["capabilities"]["pause"], true);
        assert_eq!(value["provider"]["capabilities"]["resume"], true);
        assert_eq!(value["provider"]["capabilities"]["force_start"], false);
    }

    #[test]
    fn localhost_detection_covers_the_forms_sonarr_checks_and_ipv6() {
        assert!(is_localhost_url("http://localhost:6800/rpc"));
        assert!(is_localhost_url("http://127.0.0.1:6800/rpc"));
        assert!(is_localhost_url("http://[::1]:6800/rpc"));
        assert!(!is_localhost_url("http://nas.lan:6800/rpc"));
    }
}

#[cfg(test)]
mod extism_host_stubs {
    #[unsafe(no_mangle)]
    pub extern "C" fn alloc(_len: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn config_get(_ptr: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn http_headers() -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn http_request(_request: u64, _body: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn http_status_code() -> u64 {
        200
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn length(_offset: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn length_unsafe(_offset: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn load_u64(_offset: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn load_u8(_offset: u64) -> u8 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn store_u64(_offset: u64, _value: u64) {}

    #[unsafe(no_mangle)]
    pub extern "C" fn store_u8(_offset: u64, _value: u8) {}

    #[unsafe(no_mangle)]
    pub extern "C" fn var_get(_ptr: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn var_set(_ptr: u64, _value: u64) {}
}

// ---------------------------------------------------------------------------
// `scryer:download-client/download-client@1.0.0`
// ---------------------------------------------------------------------------
//
// Transport only. Every operation above is untouched — the same URLs, headers,
// status rules and plugin state. What changed is how the host reaches them: a
// `process` export carrying the very command envelope the Preview 1 runner
// already moved over stdin/stdout, instead of a `main` reading stdin.
//
// The function table is the single source of truth for both exports, so
// `describe` and `process` cannot drift apart, and the operation semantics —
// merged failed history, scoped listings, non-destructive mark-imported — stay
// in the PDK bridge where every client shares them.

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:download-client/download-client@1.0.0",
    // The shared `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/download-client-v1.0.0"],
    // The host package is its own WIT package, so wit-bindgen asks explicitly
    // whether to generate for it. Yes: the PDK holds only a `fn` pointer and
    // the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

fn functions() -> LegacyDownloadClientFunctions {
    LegacyDownloadClientFunctions {
        describe: scryer_describe,
        add: scryer_download_add,
        list_queue: scryer_download_list_queue,
        list_history: scryer_download_list_history,
        list_completed: scryer_download_list_completed,
        list_recent_completed: None,
        control: scryer_download_control,
        mark_imported: scryer_download_mark_imported,
        // aria2 has nothing to label, so the bridge's own `Ok(())` is the
        // whole handoff (`download_client_bridge.rs:118-125`).
        mark_imported_non_destructive: None,
        status: scryer_download_status,
        test_connection: scryer_download_test_connection,
    }
}

fn build_descriptor() -> PluginDescriptor {
    legacy_download_client_descriptor(&functions())
}

fn handle_download_client_command(
    command: PluginDownloadClientCommand,
) -> PluginDownloadClientCommandResult {
    bridge_download_client_command(&functions(), command)
}

scryer_plugin_pdk::scryer_download_client_component_main!(
    descriptor = build_descriptor,
    handler = handle_download_client_command,
);
