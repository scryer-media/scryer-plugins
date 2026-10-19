//! Conformance against the real Scryer download-client host, run on the
//! RELEASE artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way the host does.
//! The generic checks stay; what is added here is this plugin's own contract
//! against a scripted MeTube: the `add` body the host must see for an HTTP
//! release, URL-keyed identity, the `history` mapping, and `delete_files`
//! never leaving the plugin unless Scryer asked for the data to go.

use std::collections::BTreeMap;

use scryer_plugin_conformance::download_client::{
    DownloadClientConformance, call_download_client, instantiate,
};
use scryer_plugin_conformance::{ConfigSource, HttpReply, HttpRoute, HttpScript, Script};
use scryer_plugin_sdk::command::{PluginDownloadClientCommand, PluginDownloadClientCommandResult};
use scryer_plugin_sdk::{
    DownloadControlAction, DownloadInputKind, DownloadItemState, HttpDownloadPhase,
    HttpSubtitleMode, PluginDownloadClientAddRequest, PluginDownloadClientControlRequest,
    PluginDownloadOutputKind, PluginDownloadRelease, PluginDownloadRouting, PluginDownloadSource,
    PluginDownloadTitle, PluginErrorCode, PluginHttpDownloadSource, PluginHttpPolicy,
    PluginHttpRelease, PluginHttpSelection, PluginResult,
};

const BASE_URL: &str = "http://metube.invalid:8081";
const PAGE_URL: &str = "https://video.invalid/watch?v=clip-1";
const OUTPUT_BASENAME: &str = "Synthetic Show - S01E01-dl-42";

#[test]
fn metube_release_wasm_conforms_to_the_download_client_host_contract() {
    let suite = suite();

    suite.run();
    assert_add_submits_the_page_url_with_the_catalogue_mapping(&suite);
    assert_an_already_queued_url_is_still_an_ok_add(&suite);
    assert_a_metube_error_reply_is_a_permanent_in_band_error(&suite);
    assert_list_queue_maps_history_rows_keyed_by_url(&suite);
    assert_list_completed_reports_paths_under_the_download_dir(&suite);
    assert_remove_sends_delete_files_only_for_history_with_remove_data(&suite);
    assert_add_reports_the_key_metube_holds_the_item_under(&suite);
    assert_add_survives_an_unreadable_history_and_types_a_refused_folder(&suite);
    assert_test_connection_validates_the_catalogue(&suite);
    assert_pause_is_an_in_band_unsupported_error(&suite);
}

fn suite() -> DownloadClientConformance {
    DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "metube")
        .wasm("metube_download_client.wasm")
        .expects_descriptor(
            &["provider", "accepted_inputs"],
            serde_json::json!(["http_resource"]),
        )
        .expects_descriptor(
            &["provider", "capabilities", "http", "transports"],
            serde_json::json!(["provider_page", "direct_file", "hls", "dash"]),
        )
}

fn assert_add_submits_the_page_url_with_the_catalogue_mapping(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script(vec![add_ok()]));

    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Add(add_request()),
    );
    let PluginDownloadClientCommandResult::Add(PluginResult::Ok(response)) = result else {
        panic!("add did not return a typed ok result: {result:?}");
    };
    assert_eq!(
        response.client_item_id, PAGE_URL,
        "MeTube keys items by URL, so the page URL is the item id"
    );
    assert_eq!(response.info_hash, None);

    let script = &store.data().script;
    let request = script
        .request_to(&format!("{BASE_URL}/add"))
        .expect("the add must POST to MeTube's add route");
    assert_eq!(request.method.as_deref(), Some("POST"));
    assert!(
        request.header("origin").is_none(),
        "a server-to-server add must not carry an Origin, or MeTube's CORS guard refuses it"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("the add body is JSON");
    assert_eq!(body["url"], PAGE_URL);
    assert_eq!(body["download_type"], "video");
    assert_eq!(body["quality"], "1080");
    assert_eq!(body["codec"], "av1");
    assert_eq!(body["format"], "any");
    assert_eq!(body["folder"], OUTPUT_BASENAME);
    assert_eq!(body["playlist_item_limit"], 1);
    assert_eq!(body["auto_start"], true);
    assert_eq!(body["subtitle_language"], "en");
    assert!(
        body.get("ytdl_options_overrides").is_none(),
        "yt-dlp overrides allow command execution in the container and are never sent"
    );
}

fn assert_an_already_queued_url_is_still_an_ok_add(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(
        &suite.wasm_path(),
        script(vec![HttpRoute::exact(
            &format!("{BASE_URL}/add"),
            HttpReply::ok(r#"{"status":"ok","msg":"Already in queue: Synthetic Episode"}"#),
        )]),
    );
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Add(add_request()),
    );
    let PluginDownloadClientCommandResult::Add(PluginResult::Ok(response)) = result else {
        panic!("an already-queued URL is the same MeTube item, not an error: {result:?}");
    };
    assert_eq!(response.client_item_id, PAGE_URL);
}

fn assert_a_metube_error_reply_is_a_permanent_in_band_error(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(
        &suite.wasm_path(),
        script(vec![HttpRoute::exact(
            &format!("{BASE_URL}/add"),
            HttpReply::ok(r#"{"status":"error","msg":"Unsupported URL"}"#),
        )]),
    );
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Add(add_request()),
    );
    let PluginDownloadClientCommandResult::Add(PluginResult::Err(error)) = result else {
        panic!("a MeTube error status must be a typed plugin error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::Permanent);
    assert!(
        error.public_message.contains("Unsupported URL"),
        "MeTube's own message reaches the operator: {}",
        error.public_message
    );
}

fn assert_list_queue_maps_history_rows_keyed_by_url(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script(vec![history()]));
    let result = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);
    let PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(items)) = result else {
        panic!("list_queue did not return a typed ok result: {result:?}");
    };

    let downloading = items
        .iter()
        .find(|item| item.client_item_id == PAGE_URL)
        .expect("the downloading row is keyed by its URL");
    // `done` also holds an earlier failed attempt at this URL. The bridge's
    // merge gives a failed history row precedence, so the plugin must not
    // report a row a fresh attempt has superseded.
    assert_eq!(
        downloading.state,
        DownloadItemState::Downloading,
        "an earlier failure at the same URL must not fail the running attempt"
    );
    assert_eq!(downloading.progress_percent, Some(42));
    assert_eq!(
        downloading.http.as_ref().and_then(|http| http.phase),
        Some(HttpDownloadPhase::Downloading)
    );
    assert_eq!(
        downloading.remote_output_path.as_deref(),
        Some("/downloads/Synthetic Show - S01E01-dl-42/Synthetic Episode.mkv")
    );

    let pending = items
        .iter()
        .find(|item| item.client_item_id == "https://video.invalid/watch?v=clip-2")
        .expect("pending rows are part of the queue");
    assert_eq!(pending.state, DownloadItemState::Queued);

    // The PDK bridge merges failed history into the queue poll, so a failure
    // in MeTube's `done` list reaches Scryer here.
    let failed = items
        .iter()
        .find(|item| item.client_item_id == "https://video.invalid/watch?v=clip-3")
        .expect("a failed done row is merged into the queue listing");
    assert_eq!(failed.state, DownloadItemState::Failed);
    assert_eq!(failed.message.as_deref(), Some("Video unavailable"));

    assert!(
        !items
            .iter()
            .any(|item| item.client_item_id == "https://video.invalid/watch?v=clip-4"),
        "a finished row belongs to the completed listing, not the queue"
    );
}

fn assert_list_completed_reports_paths_under_the_download_dir(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script(vec![history()]));
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::ListCompleted,
    );
    let PluginDownloadClientCommandResult::ListCompleted(PluginResult::Ok(downloads)) = result
    else {
        panic!("list_completed did not return a typed ok result: {result:?}");
    };
    assert_eq!(downloads.len(), 1, "only the finished row is complete");
    let finished = &downloads[0];
    assert_eq!(
        finished.client_item_id,
        "https://video.invalid/watch?v=clip-4"
    );
    assert_eq!(
        finished.dest_dir,
        "/downloads/Synthetic Show - S01E02-dl-43"
    );
    assert_eq!(
        finished.content_paths,
        vec!["/downloads/Synthetic Show - S01E02-dl-43/Synthetic Episode Two.mp4"]
    );
    assert_eq!(finished.output_kind, Some(PluginDownloadOutputKind::File));
    assert_eq!(finished.size_bytes, Some(2048));
}

fn assert_remove_sends_delete_files_only_for_history_with_remove_data(
    suite: &DownloadClientConformance,
) {
    let delete_route = || {
        HttpRoute::exact(
            &format!("{BASE_URL}/delete"),
            HttpReply::ok(r#"{"status":"ok"}"#),
        )
    };
    // (is_history, remove_data, where MeTube holds the item, expected where,
    // expected delete_files). The fourth case is a failed item: Scryer sees it
    // in its queue poll, MeTube holds it in `done`, and `where: "queue"` would
    // be a silent no-op there.
    let cases: [(bool, bool, Holder, &str, Option<bool>); 4] = [
        (false, true, Holder::Queue, "queue", None),
        (true, false, Holder::Done, "done", Some(false)),
        (true, true, Holder::Done, "done", Some(true)),
        (false, true, Holder::Done, "done", Some(false)),
    ];
    for (is_history, remove_data, holder, expected_where, expected_delete_files) in cases {
        let (mut store, plugin) = instantiate(
            &suite.wasm_path(),
            script(vec![holder_history(holder), delete_route()]),
        );
        let result = call_download_client(
            &mut store,
            &plugin,
            PluginDownloadClientCommand::Control(PluginDownloadClientControlRequest {
                action: DownloadControlAction::Remove,
                client_item_id: PAGE_URL.to_string(),
                remove_data,
                is_history,
            }),
        );
        assert!(
            matches!(
                result,
                PluginDownloadClientCommandResult::Control(PluginResult::Ok(()))
            ),
            "remove must succeed against an ok reply: {result:?}"
        );
        let request = store
            .data()
            .script
            .request_to(&format!("{BASE_URL}/delete"))
            .expect("remove must POST to MeTube's delete route");
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("the delete body is JSON");
        assert_eq!(body["ids"], serde_json::json!([PAGE_URL]));
        assert_eq!(body["where"], expected_where);
        assert_eq!(
            body.get("delete_files")
                .and_then(serde_json::Value::as_bool),
            expected_delete_files,
            "delete_files travels only with a history clear, and only as Scryer asked"
        );
    }

    // A history clear while MeTube runs a fresh attempt at the same URL must
    // not cancel that attempt, and must not send any delete at all.
    let (mut store, plugin) = instantiate(
        &suite.wasm_path(),
        script(vec![holder_history(Holder::Both), delete_route()]),
    );
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Control(PluginDownloadClientControlRequest {
            action: DownloadControlAction::Remove,
            client_item_id: PAGE_URL.to_string(),
            remove_data: true,
            is_history: true,
        }),
    );
    assert!(
        matches!(
            result,
            PluginDownloadClientCommandResult::Control(PluginResult::Ok(()))
        ),
        "{result:?}"
    );
    assert!(
        store
            .data()
            .script
            .request_to(&format!("{BASE_URL}/delete"))
            .is_none(),
        "no delete may reach MeTube for an item it is downloading again"
    );
}

fn assert_add_reports_the_key_metube_holds_the_item_under(suite: &DownloadClientConformance) {
    // yt-dlp keyed the item by a `webpage_url` that differs from the submitted
    // page URL; the item id must be the key every later `delete` has to name.
    const CANONICAL: &str = "https://video.invalid/watch?v=clip-1&canonical=1";
    let (mut store, plugin) = instantiate(
        &suite.wasm_path(),
        script(vec![
            add_ok(),
            HttpRoute::exact(
                &format!("{BASE_URL}/history"),
                HttpReply::ok(&format!(
                    r#"{{"done":[],"pending":[],"queue":[
  {{"id":"clip-1","title":"Synthetic Episode","url":"{CANONICAL}","status":"preparing"}}]}}"#
                )),
            ),
        ]),
    );
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Add(add_request()),
    );
    let PluginDownloadClientCommandResult::Add(PluginResult::Ok(response)) = result else {
        panic!("add did not return a typed ok result: {result:?}");
    };
    assert_eq!(response.client_item_id, CANONICAL);
}

fn assert_add_survives_an_unreadable_history_and_types_a_refused_folder(
    suite: &DownloadClientConformance,
) {
    // A history read after an ok add that cannot be served still leaves the
    // add a success, keyed by the page URL.
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script(vec![add_ok()]));
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Add(add_request()),
    );
    let PluginDownloadClientCommandResult::Add(PluginResult::Ok(response)) = result else {
        panic!("an unreadable history must not fail an add MeTube accepted: {result:?}");
    };
    assert_eq!(response.client_item_id, PAGE_URL);

    // A sub-folder MeTube will not create is configuration, not the release.
    let (mut store, plugin) = instantiate(
        &suite.wasm_path(),
        script(vec![HttpRoute::exact(
            &format!("{BASE_URL}/add"),
            HttpReply::ok(
                r#"{"status":"error","msg":"A folder for the download was specified but CUSTOM_DIRS is not true in the configuration."}"#,
            ),
        )]),
    );
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Add(add_request()),
    );
    let PluginDownloadClientCommandResult::Add(PluginResult::Err(error)) = result else {
        panic!("a refused folder must be a typed error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
}

fn assert_test_connection_validates_the_catalogue(suite: &DownloadClientConformance) {
    let version = || {
        HttpRoute::exact(
            &format!("{BASE_URL}/version"),
            HttpReply::ok(r#"{"yt-dlp":"2026.10.01","version":"2026.10.09"}"#),
        )
    };
    let formats = |codecs: &str| {
        HttpRoute::exact(
            &format!("{BASE_URL}/formats"),
            HttpReply::ok(&format!(
                r#"{{"download_type":{{"default":"video","options":[{{"id":"video","text":"Video",
  "codec":{{"default":"auto","options":[{codecs}]}},
  "format":{{"default":"any","options":[
    {{"id":"any","text":"Any","quality":{{"default":"best","options":[{QUALITIES}]}}}},
    {{"id":"mp4","text":"MP4","quality":{{"default":"best","options":[{QUALITIES}]}}}},
    {{"id":"ios","text":"iOS","quality":{{"default":"best","options":[{QUALITIES}]}}}}]}}}}]}}}}"#
            )),
        )
    };
    const QUALITIES: &str = r#"{"id":"best"},{"id":"2160"},{"id":"1440"},{"id":"1080"},{"id":"720"},{"id":"480"},{"id":"360"},{"id":"240"},{"id":"worst"}"#;
    const ALL_CODECS: &str =
        r#"{"id":"auto"},{"id":"h264"},{"id":"h265"},{"id":"av1"},{"id":"vp9"}"#;

    let (mut store, plugin) = instantiate(
        &suite.wasm_path(),
        script(vec![version(), formats(ALL_CODECS)]),
    );
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::TestConnection,
    );
    let PluginDownloadClientCommandResult::TestConnection(PluginResult::Ok(version_text)) = result
    else {
        panic!("a current catalogue must pass the connection test: {result:?}");
    };
    assert!(version_text.contains("2026.10.09"), "{version_text}");

    let (mut store, plugin) = instantiate(
        &suite.wasm_path(),
        script(vec![
            version(),
            formats(r#"{"id":"auto"},{"id":"h264"},{"id":"h265"},{"id":"vp9"}"#),
        ]),
    );
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::TestConnection,
    );
    let PluginDownloadClientCommandResult::TestConnection(PluginResult::Err(error)) = result else {
        panic!("a catalogue without a value the plugin sends must fail the test: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("codec av1"), "{error:?}");
}

#[derive(Clone, Copy)]
enum Holder {
    Queue,
    Done,
    Both,
}

/// A `history` reply holding `PAGE_URL` in the given list(s).
fn holder_history(holder: Holder) -> HttpRoute {
    let active = format!(r#"{{"url":"{PAGE_URL}","status":"downloading"}}"#);
    let done = format!(r#"{{"url":"{PAGE_URL}","status":"error","msg":"Video unavailable"}}"#);
    let (queue, done) = match holder {
        Holder::Queue => (active, String::new()),
        Holder::Done => (String::new(), done),
        Holder::Both => (active, done),
    };
    HttpRoute::exact(
        &format!("{BASE_URL}/history"),
        HttpReply::ok(&format!(
            r#"{{"queue":[{queue}],"done":[{done}],"pending":[]}}"#
        )),
    )
}

fn assert_pause_is_an_in_band_unsupported_error(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script(vec![]));
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Control(PluginDownloadClientControlRequest {
            action: DownloadControlAction::Pause,
            client_item_id: PAGE_URL.to_string(),
            remove_data: false,
            is_history: false,
        }),
    );
    let PluginDownloadClientCommandResult::Control(PluginResult::Err(error)) = result else {
        panic!("pause must report an in-band error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);
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
            source_title: Some("Synthetic Show S01E01".to_string()),
            source_password: None,
            password_candidates: Vec::new(),
            http: Some(PluginHttpDownloadSource {
                release: PluginHttpRelease {
                    provider: "synthetic".to_string(),
                    item_id: "clip-1".to_string(),
                    page_url: PAGE_URL.to_string(),
                    ..Default::default()
                },
                selected: PluginHttpSelection {
                    max_height: Some(1080),
                    video_codec_preference: vec!["av1".to_string(), "h264".to_string()],
                    container_preference: vec!["mkv".to_string()],
                    subtitle_languages: vec!["en".to_string()],
                    subtitle_mode: HttpSubtitleMode::SidecarPreferred,
                    ..Default::default()
                },
                policy: PluginHttpPolicy {
                    output_basename: OUTPUT_BASENAME.to_string(),
                    expected_duration_seconds: Some(1320),
                    ..Default::default()
                },
            }),
        },
        release: PluginDownloadRelease {
            download_id: Some("dl-42".to_string()),
            release_title: Some("Synthetic Show S01E01".to_string()),
            import_purpose: None,
            is_recent: None,
            season_pack: None,
            indexer_name: Some("synthetic".to_string()),
            info_hash_hint: None,
            info_hash_v1: None,
            info_hash_v2: None,
            seed_goal_ratio: None,
            seed_goal_seconds: None,
        },
        title: PluginDownloadTitle {
            title_id: Some("title-1".to_string()),
            title_name: "Synthetic Show".to_string(),
            media_facet: "series".to_string(),
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

fn add_ok() -> HttpRoute {
    HttpRoute::exact(
        &format!("{BASE_URL}/add"),
        HttpReply::ok(r#"{"status":"ok"}"#),
    )
}

/// A `history` reply with one row per state the plugin maps differently:
/// downloading, pending, failed and finished.
fn history() -> HttpRoute {
    HttpRoute::exact(
        &format!("{BASE_URL}/history"),
        HttpReply::ok(
            r#"{
  "done": [
    {"id": "clip-3", "title": "Synthetic Episode Three", "url": "https://video.invalid/watch?v=clip-3",
     "status": "error", "msg": "", "error": "Video unavailable", "filename": null},
    {"id": "clip-4", "title": "Synthetic Episode Two", "url": "https://video.invalid/watch?v=clip-4",
     "status": "finished", "size": 2048, "timestamp": 1760000000.5,
     "folder": "Synthetic Show - S01E02-dl-43", "filename": "Synthetic Episode Two.mp4"},
    {"id": "clip-1", "title": "Synthetic Episode", "url": "https://video.invalid/watch?v=clip-1",
     "status": "error", "msg": "an earlier attempt failed", "filename": null}
  ],
  "queue": [
    {"id": "clip-1", "title": "Synthetic Episode", "url": "https://video.invalid/watch?v=clip-1",
     "status": "downloading", "percent": 42.7, "speed": 1200.5, "eta": 30.0, "size": 4096.0,
     "folder": "Synthetic Show - S01E01-dl-42", "filename": "Synthetic Episode.mkv"}
  ],
  "pending": [
    {"id": "clip-2", "title": "Synthetic Episode Pending", "url": "https://video.invalid/watch?v=clip-2",
     "status": "pending"}
  ]
}"#,
        ),
    )
}

/// A MeTube stand-in behind the configured base URL. Routes are exact, so an
/// unrouted request fails loudly instead of answering with another body.
fn script(routes: Vec<HttpRoute>) -> Script {
    let mut config = BTreeMap::new();
    config.insert("base_url".to_string(), BASE_URL.to_string());
    config.insert("download_dir".to_string(), "/downloads".to_string());
    config.insert("subfolder_per_download".to_string(), "true".to_string());

    Script {
        config: ConfigSource::Resolved(config),
        http: HttpScript::Routed(routes),
        log_http_method: true,
        ..Script::default()
    }
}
