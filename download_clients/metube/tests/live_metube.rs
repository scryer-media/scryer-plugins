//! A live end-to-end check: the RELEASE artifact drives a real MeTube, which
//! really downloads a video.
//!
//! Ignored by default, because it needs a MeTube the developer runs and the
//! upstream video site. Run it with:
//!
//! ```text
//! METUBE_LIVE_URL=http://127.0.0.1:18081 \
//! METUBE_LIVE_DOWNLOAD_DIR=/host/path/mounted/as/downloads \
//!   cargo nextest run --locked --test live_metube --run-ignored all --no-capture
//! ```
//!
//! `METUBE_LIVE_DOWNLOAD_DIR` is the host directory mounted as MeTube's
//! `/downloads`, so the test can check the file the plugin reports really
//! landed on disk.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use scryer_plugin_conformance::download_client::{
    DownloadClientConformance, call_download_client, instantiate,
};
use scryer_plugin_conformance::{ConfigSource, HttpScript, Script};
use scryer_plugin_sdk::command::{PluginDownloadClientCommand, PluginDownloadClientCommandResult};
use scryer_plugin_sdk::{
    DownloadControlAction, DownloadInputKind, DownloadItemState, PluginCompletedDownload,
    PluginDownloadClientAddRequest, PluginDownloadClientControlRequest, PluginDownloadItem,
    PluginDownloadRelease, PluginDownloadRouting, PluginDownloadSource, PluginDownloadTitle,
    PluginHttpDownloadSource, PluginHttpPolicy, PluginHttpRelease, PluginHttpSelection,
    PluginResult,
};

const PAGE_URL: &str = "https://www.youtube.com/watch?v=rzR6VPALFo4";
const OUTPUT_BASENAME: &str = "Scryer Live Check - rzR6VPALFo4";
const CONTAINER_DOWNLOAD_DIR: &str = "/downloads";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const DOWNLOAD_BOUND: Duration = Duration::from_secs(10 * 60);
const MIN_FILE_BYTES: u64 = 1024 * 1024;

#[test]
#[ignore = "needs a local MeTube (METUBE_LIVE_URL, METUBE_LIVE_DOWNLOAD_DIR) and the upstream video site"]
fn metube_really_downloads_a_video_through_the_release_artifact() {
    let base_url =
        std::env::var("METUBE_LIVE_URL").expect("METUBE_LIVE_URL must name the MeTube base URL");
    let host_download_dir = PathBuf::from(
        std::env::var("METUBE_LIVE_DOWNLOAD_DIR")
            .expect("METUBE_LIVE_DOWNLOAD_DIR must name the host dir mounted as /downloads"),
    );
    assert!(
        host_download_dir.is_dir(),
        "METUBE_LIVE_DOWNLOAD_DIR is not a directory: {}",
        host_download_dir.display()
    );

    let wasm = DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "metube")
        .wasm("metube_download_client.wasm")
        .wasm_path();
    let (mut store, plugin) = instantiate(&wasm, live_script(&base_url));
    let mut call = |command| call_download_client(&mut store, &plugin, command);

    // Connection and status.
    let result = call(PluginDownloadClientCommand::TestConnection);
    let PluginDownloadClientCommandResult::TestConnection(PluginResult::Ok(message)) = result
    else {
        panic!("test_connection against live MeTube failed: {result:?}");
    };
    println!("[live] test_connection: {message}");

    let result = call(PluginDownloadClientCommand::Status);
    let PluginDownloadClientCommandResult::Status(PluginResult::Ok(status)) = result else {
        panic!("status against live MeTube failed: {result:?}");
    };
    println!(
        "[live] status: version={:?} remote_output_roots={:?} warnings={:?}",
        status.version, status.remote_output_roots, status.warnings
    );

    // Submit.
    let result = call(PluginDownloadClientCommand::Add(add_request()));
    let PluginDownloadClientCommandResult::Add(PluginResult::Ok(added)) = result else {
        panic!("add against live MeTube failed: {result:?}");
    };
    println!("[live] add: client_item_id={}", added.client_item_id);
    assert_eq!(
        added.client_item_id, PAGE_URL,
        "the plugin's item id is the page URL"
    );
    let item_id = added.client_item_id;

    // Poll until the item, identified by its URL, is complete or failed.
    let started = Instant::now();
    let deadline = started + DOWNLOAD_BOUND;
    let completed: PluginCompletedDownload = loop {
        let queue = match call(PluginDownloadClientCommand::ListQueue) {
            PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(items)) => items,
            other => panic!("list_queue against live MeTube failed: {other:?}"),
        };
        let done = match call(PluginDownloadClientCommand::ListCompleted) {
            PluginDownloadClientCommandResult::ListCompleted(PluginResult::Ok(items)) => items,
            other => panic!("list_completed against live MeTube failed: {other:?}"),
        };

        let elapsed = started.elapsed().as_secs_f32();
        if let Some(finished) = done.into_iter().find(|item| item.client_item_id == item_id) {
            println!(
                "[live] t={elapsed:>6.1}s completed: dest_dir={} content_paths={:?} size_bytes={:?}",
                finished.dest_dir, finished.content_paths, finished.size_bytes
            );
            break finished;
        }
        match queue.iter().find(|item| item.client_item_id == item_id) {
            Some(item) => {
                print_progress(elapsed, item);
                assert_ne!(
                    item.state,
                    DownloadItemState::Failed,
                    "MeTube reported the download as failed: {:?}",
                    item.message
                );
            }
            None => println!("[live] t={elapsed:>6.1}s item not yet listed"),
        }

        assert!(
            Instant::now() < deadline,
            "the download did not finish within {DOWNLOAD_BOUND:?}"
        );
        std::thread::sleep(POLL_INTERVAL);
    };

    // The file the plugin reports is real, and big enough to be a video.
    assert_eq!(
        completed.content_paths.len(),
        1,
        "exactly one content file: {:?}",
        completed.content_paths
    );
    let content_path = &completed.content_paths[0];
    let relative = content_path
        .strip_prefix(&format!("{CONTAINER_DOWNLOAD_DIR}/"))
        .unwrap_or_else(|| panic!("content path is not under /downloads: {content_path}"));
    let host_file = host_download_dir.join(relative);
    let metadata = std::fs::metadata(&host_file)
        .unwrap_or_else(|error| panic!("host file {} is missing: {error}", host_file.display()));
    assert!(metadata.is_file(), "{} is not a file", host_file.display());
    println!(
        "[live] host file: {} ({} bytes)",
        host_file.display(),
        metadata.len()
    );
    assert!(
        metadata.len() > MIN_FILE_BYTES,
        "the downloaded file is only {} bytes",
        metadata.len()
    );

    // Clearing the history row keeps the data.
    let result = call(PluginDownloadClientCommand::Control(
        PluginDownloadClientControlRequest {
            action: DownloadControlAction::Remove,
            client_item_id: item_id.clone(),
            remove_data: false,
            is_history: true,
        },
    ));
    assert!(
        matches!(
            result,
            PluginDownloadClientCommandResult::Control(PluginResult::Ok(()))
        ),
        "remove of the history row failed: {result:?}"
    );
    let done = match call(PluginDownloadClientCommand::ListCompleted) {
        PluginDownloadClientCommandResult::ListCompleted(PluginResult::Ok(items)) => items,
        other => panic!("list_completed after remove failed: {other:?}"),
    };
    assert!(
        !done.iter().any(|item| item.client_item_id == item_id),
        "the history row must be gone after remove"
    );
    assert!(
        host_file.is_file(),
        "remove with remove_data=false must leave {} in place",
        host_file.display()
    );
    println!("[live] removed history row; file still present");
}

fn print_progress(elapsed: f32, item: &PluginDownloadItem) {
    println!(
        "[live] t={elapsed:>6.1}s state={:?} phase={:?} percent={:?} message={:?}",
        item.state,
        item.http.as_ref().and_then(|http| http.phase),
        item.progress_percent,
        item.message
    );
}

fn live_script(base_url: &str) -> Script {
    let mut config = BTreeMap::new();
    config.insert("base_url".to_string(), base_url.to_string());
    config.insert(
        "download_dir".to_string(),
        CONTAINER_DOWNLOAD_DIR.to_string(),
    );
    config.insert("subfolder_per_download".to_string(), "true".to_string());
    Script {
        config: ConfigSource::Resolved(config),
        http: HttpScript::Live,
        log_http_method: true,
        ..Script::default()
    }
}

fn add_request() -> PluginDownloadClientAddRequest {
    PluginDownloadClientAddRequest {
        source: PluginDownloadSource {
            kind: DownloadInputKind::HttpResource,
            download_url: None,
            magnet_uri: None,
            torrent_bytes_base64: None,
            torrent_url: None,
            torrent_file_name: None,
            torrent_content_type: None,
            nzb_bytes_base64: None,
            nzb_file_name: None,
            nzb_content_type: None,
            source_title: Some(OUTPUT_BASENAME.to_string()),
            source_password: None,
            password_candidates: Vec::new(),
            http: Some(PluginHttpDownloadSource {
                release: PluginHttpRelease {
                    provider: "youtube".to_string(),
                    item_id: "rzR6VPALFo4".to_string(),
                    page_url: PAGE_URL.to_string(),
                    ..Default::default()
                },
                // A plain selection: no height ceiling, no codec or container
                // preference, no subtitles.
                selected: PluginHttpSelection::default(),
                policy: PluginHttpPolicy {
                    output_basename: OUTPUT_BASENAME.to_string(),
                    ..Default::default()
                },
            }),
        },
        release: PluginDownloadRelease {
            download_id: None,
            release_title: Some(OUTPUT_BASENAME.to_string()),
            import_purpose: None,
            is_recent: None,
            season_pack: None,
            indexer_name: None,
            info_hash_hint: None,
            info_hash_v1: None,
            info_hash_v2: None,
            seed_goal_ratio: None,
            seed_goal_seconds: None,
        },
        title: PluginDownloadTitle {
            title_id: None,
            title_name: "Scryer Live Check".to_string(),
            media_facet: "movie".to_string(),
            title_slug: None,
            year: None,
            language: None,
            network: None,
            tags: Vec::new(),
        },
        routing: PluginDownloadRouting::default(),
        torrent: None,
    }
}
