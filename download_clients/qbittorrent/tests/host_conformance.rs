//! Conformance against the real Scryer download-client host, run on the
//! RELEASE artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way
//! `crates/scryer-plugins/src/wasmtime_host/download_client_component_host.rs`
//! does.
//!
//! # What is specific to qBittorrent
//!
//! conformance: bespoke — this is the only client in the family that carries a
//! *session*. It authenticates once, stores the SID cookie in plugin state, and
//! must reuse it across instances, because a component instance does not
//! survive a `process` call. Three of the checks below have no counterpart in
//! any other client: the login-and-listing URL contract, the cookie outliving
//! the instance that stored it, and `test_connection` clearing the session
//! before it probes. They replace the family's generic
//! `ProcessReachesHostServices` and `RefusedHostStaysInBand` checks, which they
//! subsume and sharpen.

use std::collections::BTreeMap;

use scryer_plugin_conformance::download_client::{
    Check, DownloadClientConformance, call_download_client, instantiate,
};
use scryer_plugin_conformance::{
    ConfigSource, HttpReply, HttpRoute, HttpScript, Script, StateSource,
};
use scryer_plugin_sdk::command::{PluginDownloadClientCommand, PluginDownloadClientCommandResult};
use scryer_plugin_sdk::{
    DownloadControlAction, PluginDownloadClientControlRequest, PluginErrorCode, PluginResult,
};

/// The plugin normalises `base_url` into `<base>/api/v2`, so every scripted URL
/// below is the exact string the migrated artifact must still produce.
const BASE_URL: &str = "http://qbittorrent.invalid:8080";
const API_ROOT: &str = "http://qbittorrent.invalid:8080/api/v2";
/// `var::get::<String>` decodes JSON, so the stored cookie is a JSON string.
const COOKIE_STATE_KEY: &str = "qbittorrent.sid";
const SESSION_COOKIE: &str = "SID=conformance-session";

#[test]
fn qbittorrent_release_wasm_conforms_to_the_download_client_host_contract() {
    let suite = suite();

    suite.assert_artifact_is_a_component();
    suite.assert_world_conformance();
    suite.assert_describe_returns_a_download_client_descriptor();
    assert_list_queue_logs_in_and_uses_the_configured_base_url(&suite);
    assert_the_session_cookie_outlives_the_instance_that_stored_it(&suite);
    assert_test_connection_clears_the_session_through_state_delete(&suite);
    assert_a_refused_host_service_stays_in_band(&suite);
    assert_an_unsupported_control_action_is_in_band(&suite);
    assert_a_rejected_login_names_the_credentials(&suite);
    assert_add_options_across_qbittorrent_versions(&suite);
    assert_legacy_title_tag_migration(&suite);
    suite.assert_another_family_is_an_invocation_error();
}

fn suite() -> DownloadClientConformance {
    DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "qbittorrent")
        .expects_descriptor(
            &["provider", "capabilities", "mark_imported_non_destructive"],
            serde_json::json!(true),
        )
        .expects_descriptor(
            &["provider", "accepted_inputs"],
            serde_json::json!(["magnet_uri", "torrent_url", "torrent_bytes", "torrent_file"]),
        )
        // Both are subsumed by this client's own, sharper session checks below.
        .without(Check::ProcessReachesHostServices)
        .without(Check::RefusedHostStaysInBand)
}

/// The client's configuration, its login, and its listing request all travel
/// over the one `host-call` import, and the URLs are the pre-migration ones.
fn assert_list_queue_logs_in_and_uses_the_configured_base_url(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), authenticated_script());

    let result = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);
    let PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(items)) = result else {
        panic!("list_queue did not return a typed ok result: {result:?}");
    };
    assert!(items.is_empty(), "the scripted client has no torrents");

    let calls = &store.data().script.calls;
    assert!(
        calls.iter().any(|call| call == "config_get:base_url"),
        "the client must read its base URL through host services: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:POST {API_ROOT}/auth/login")),
        "an empty session must log in first: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call
            == &format!("http:GET {API_ROOT}/torrents/info?sort=added_on&reverse=true&filter=all")),
        "the configured base URL and query must be used verbatim: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("state_set:{COOKIE_STATE_KEY}")),
        "the SID cookie must be written to plugin state: {calls:?}"
    );
}

/// The one service this family leans on that a catalog provider does not.
///
/// A component instance does not survive a `process` call, so a client that
/// authenticates once and reuses its session must keep the cookie in plugin
/// state. The host backs every invocation of a configured client with one
/// `CommandHost`, and therefore one state map — modelled here by carrying the
/// first invocation's map into a second, freshly instantiated component.
///
/// The proof is negative as well as positive: the second invocation must send
/// `Cookie: SID=…` *and* must not call `/auth/login` again.
fn assert_the_session_cookie_outlives_the_instance_that_stored_it(
    suite: &DownloadClientConformance,
) {
    let wasm_path = suite.wasm_path();
    let (mut first_store, first_plugin) = instantiate(&wasm_path, authenticated_script());
    let first = call_download_client(
        &mut first_store,
        &first_plugin,
        PluginDownloadClientCommand::ListQueue,
    );
    assert!(
        matches!(
            first,
            PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(_))
        ),
        "the first invocation must succeed before its session can be reused: {first:?}"
    );

    let carried_state = first_store.data().script.stored_state();
    assert_eq!(
        carried_state
            .get(COOKIE_STATE_KEY)
            .map(|value| String::from_utf8_lossy(value).to_string())
            .as_deref(),
        Some(format!("\"{SESSION_COOKIE}\"").as_str()),
        "the login cookie must be the value handed to StateSet"
    );

    // A brand-new instance, the same state map — exactly what the host does on
    // the next poll of the same configured client.
    let mut script = authenticated_script();
    script.state = StateSource::Stored(carried_state);
    let (mut second_store, second_plugin) = instantiate(&wasm_path, script);
    let second = call_download_client(
        &mut second_store,
        &second_plugin,
        PluginDownloadClientCommand::ListQueue,
    );
    assert!(
        matches!(
            second,
            PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(_))
        ),
        "the reused session must still list the queue: {second:?}"
    );

    let script = &second_store.data().script;
    assert!(
        script.made_call(&format!("state_get:{COOKIE_STATE_KEY}")),
        "the second invocation must read the cookie back out of state: {:?}",
        script.calls
    );
    assert!(
        !script.made_call(&format!("http:POST {API_ROOT}/auth/login")),
        "a stored session must not be re-authenticated: {:?}",
        script.calls
    );
    assert!(
        sent_session_cookie(script),
        "the reused session must travel as a Cookie header: {:?}",
        script.requests
    );
}

/// `test_connection` deliberately discards the session before probing, so it is
/// the operation that pins `StateDelete` crossing the same import.
fn assert_test_connection_clears_the_session_through_state_delete(
    suite: &DownloadClientConformance,
) {
    let mut script = authenticated_script();
    let mut state = script.stored_state();
    state.insert(
        COOKIE_STATE_KEY.to_string(),
        format!("\"{SESSION_COOKIE}\"").into_bytes(),
    );
    script.state = StateSource::Stored(state);
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script);

    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::TestConnection,
    );
    let PluginDownloadClientCommandResult::TestConnection(PluginResult::Ok(version)) = result
    else {
        panic!("test_connection did not return a typed ok result: {result:?}");
    };
    assert_eq!(version, "v5.2.0");

    let calls = &store.data().script.calls;
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("state_delete:{COOKIE_STATE_KEY}")),
        "test_connection must clear the stored session: {calls:?}"
    );
    // Deleting the session is not cosmetic: the probe that follows must
    // re-authenticate rather than ride the cookie it was handed, which is the
    // whole point of `test_connection` proving credentials rather than reach.
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:POST {API_ROOT}/auth/login")),
        "test_connection must re-authenticate after clearing the session: {calls:?}"
    );
    let delete_index = calls
        .iter()
        .position(|call| call == &format!("state_delete:{COOKIE_STATE_KEY}"))
        .expect("state_delete was asserted above");
    let login_index = calls
        .iter()
        .position(|call| call == &format!("http:POST {API_ROOT}/auth/login"))
        .expect("login was asserted above");
    assert!(
        delete_index < login_index,
        "the session must be cleared before the probe, not after: {calls:?}"
    );
}

/// Capability availability is in-band. A host that refuses a service answers
/// through the response, never through `host-error`, and the client must
/// surface that as a typed plugin error rather than a world-level invocation
/// failure — otherwise the host loses the plugin's own diagnosis.
fn assert_a_refused_host_service_stays_in_band(suite: &DownloadClientConformance) {
    let mut script = authenticated_script();
    script.http = HttpScript::Refused;
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script);

    let result = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);
    let PluginDownloadClientCommandResult::ListQueue(PluginResult::Err(error)) = result else {
        panic!("a refused HTTP service must be a typed plugin error: {result:?}");
    };
    assert!(
        !error.public_message.is_empty(),
        "the client keeps its own diagnosis on the way out"
    );
}

/// qBittorrent has no force-start, and the pre-migration plugin answered that
/// in-band. The component must not turn it into an `invocation-error`.
fn assert_an_unsupported_control_action_is_in_band(suite: &DownloadClientConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), authenticated_script());
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Control(PluginDownloadClientControlRequest {
            client_item_id: "0000000000000000000000000000000000000000".to_string(),
            action: DownloadControlAction::ForceStart,
            remove_data: false,
            is_history: false,
        }),
    );
    let PluginDownloadClientCommandResult::Control(PluginResult::Err(error)) = result else {
        panic!("force_start must report an in-band error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);
}

/// qBittorrent answers a wrong username or password with `200 Fails.` and no
/// cookie; the operator must see that the credentials were refused, not a
/// generic failure the host retries.
fn assert_a_rejected_login_names_the_credentials(suite: &DownloadClientConformance) {
    let mut script = authenticated_script();
    let HttpScript::Routed(routes) = &mut script.http else {
        unreachable!("the authenticated script is routed");
    };
    routes[0] = HttpRoute::exact(&format!("{API_ROOT}/auth/login"), HttpReply::ok("Fails."));
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script);

    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::TestConnection,
    );
    let PluginDownloadClientCommandResult::TestConnection(PluginResult::Err(error)) = result else {
        panic!("a refused login must be a typed plugin error: {result:?}");
    };
    assert_eq!(
        error.code,
        PluginErrorCode::from("invalid_username_password"),
        "{error:?}"
    );
    assert!(
        !error.public_message.contains("secret"),
        "the public message never carries the password: {error:?}"
    );
}

/// Exercise both encodings through the shipping component, then read the
/// options using each released server's parameter names. Unknown parameters
/// are ignored by TorrentsController::addAction in all three versions.
fn assert_add_options_across_qbittorrent_versions(suite: &DownloadClientConformance) {
    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
    for (version, paused_key, checking_key, cookie) in [
        ("4.6.7", "paused", "skip_checking", "SID=session"),
        ("5.2.4", "stopped", "skip_checking", "QBT_SID_8080=session"),
        ("5.3.0", "stopped", "seedMode", "QBT_SID_8080=session"),
    ] {
        for multipart in [false, true] {
            for enabled in [false, true] {
                let mut script = authenticated_script();
                let HttpScript::Routed(routes) = &mut script.http else {
                    unreachable!("the authenticated script is routed");
                };
                routes[0] = HttpRoute::exact(
                    &format!("{API_ROOT}/auth/login"),
                    HttpReply::new(
                        if version.starts_with('4') { 200 } else { 204 },
                        if version.starts_with('4') {
                            b"Ok.".to_vec()
                        } else {
                            Vec::new()
                        },
                    )
                    .with_header("Set-Cookie", &format!("{cookie}; HttpOnly; path=/")),
                );
                routes[1] = HttpRoute::exact(
                    &format!("{API_ROOT}/torrents/info?sort=added_on&reverse=true&filter=all"),
                    HttpReply::ok(&format!(
                        r#"[{{"hash":"{HASH}","name":"Example","state":"downloading"}}]"#
                    )),
                );
                routes.push(HttpRoute::exact(
                    &format!("{API_ROOT}/torrents/add"),
                    HttpReply::ok(&if version.starts_with('4') {
                        "Ok.".to_string()
                    } else {
                        format!(r#"{{"success_count":1,"failure_count":0,"pending_count":0,"added_torrent_ids":["{HASH}"]}}"#)
                    }),
                ));
                let source = if multipart {
                    serde_json::json!({
                        "kind": "torrent_bytes",
                        "torrent_bytes_base64": "ZDQ6aW5mb2Q0Om5hbWU0OnRlc3RlZQ=="
                    })
                } else {
                    serde_json::json!({"kind":"magnet_uri", "magnet_uri":format!("magnet:?xt=urn:btih:{HASH}")})
                };
                let request = serde_json::from_value(serde_json::json!({
                    "source": source,
                    "release": {"info_hash_hint": HASH},
                    "title": {"title_name":"Example", "media_facet":"movie", "tags":[]},
                    "routing": {"isolation":[], "post_import_isolation":[]},
                    "torrent": {
                        "auto_management": enabled,
                        "initial_state": if enabled { "stopped" } else { "started" },
                        "skip_checking": enabled,
                        "force_start": enabled
                    }
                }))
                .expect("valid add request");
                let (mut store, plugin) = instantiate(&suite.wasm_path(), script);
                let result = call_download_client(
                    &mut store,
                    &plugin,
                    PluginDownloadClientCommand::Add(request),
                );
                let PluginDownloadClientCommandResult::Add(PluginResult::Ok(added)) = result else {
                    panic!("{version}, multipart={multipart}, enabled={enabled}: {result:?}");
                };
                assert_eq!(added.client_item_id, HASH);
                let sent = store
                    .data()
                    .script
                    .requests
                    .iter()
                    .find(|request| request.url == format!("{API_ROOT}/torrents/add"))
                    .expect("add reached HTTP host");
                assert_eq!(sent.method.as_deref(), Some("POST"));
                assert_eq!(sent.header("cookie"), Some(cookie));
                let body = String::from_utf8_lossy(&sent.body);
                let fields: BTreeMap<&str, &str> = if multipart {
                    assert!(
                        sent.header("content-type")
                            .unwrap()
                            .starts_with("multipart/form-data;")
                    );
                    body.split("\r\n")
                        .collect::<Vec<_>>()
                        .windows(3)
                        .filter_map(|lines| {
                            Some((
                                lines[0]
                                    .strip_prefix("Content-Disposition: form-data; name=\"")?
                                    .strip_suffix('"')?,
                                lines[2],
                            ))
                        })
                        .collect()
                } else {
                    assert_eq!(
                        sent.header("content-type"),
                        Some("application/x-www-form-urlencoded")
                    );
                    body.split('&')
                        .filter_map(|field| field.split_once('='))
                        .collect()
                };
                for key in ["autoTMM", paused_key, checking_key, "forced"] {
                    assert_eq!(
                        fields.get(key).copied(),
                        Some(if enabled { "true" } else { "false" }),
                        "{version}, multipart={multipart}, key={key}"
                    );
                }
            }
        }
    }
}

fn assert_legacy_title_tag_migration(suite: &DownloadClientConformance) {
    let legacy = (0..405)
        .map(|index| format!("scryer-title-00000000-0000-4000-8000-{index:012x}"))
        .collect::<Vec<_>>();
    let mut tags = legacy.clone();
    tags.extend(
        [
            "user-label",
            "scryer:imported",
            "scryer:routing-category:tv",
            "scryer-title-not-a-uuid",
            "scryer-tag-favorites",
        ]
        .into_iter()
        .map(str::to_string),
    );
    let script_for = |inventory_status, delete_status| {
        let mut script = authenticated_script();
        let HttpScript::Routed(routes) = &mut script.http else {
            unreachable!()
        };
        routes[1] = HttpRoute::exact(
            &format!("{API_ROOT}/torrents/info?sort=added_on&reverse=true&filter=all"),
            HttpReply::ok(&serde_json::json!([
                {"hash":"aaaa", "name":"Synthetic torrent", "state":"downloading", "tags":format!("{},user-label", legacy[0])},
                {"hash":"bbbb", "name":"Unrelated torrent", "state":"downloading", "tags":"user-label"}
            ]).to_string()),
        );
        routes[3] = HttpRoute::exact(
            &format!("{API_ROOT}/torrents/tags"),
            HttpReply::new(inventory_status, serde_json::to_vec(&tags).unwrap()),
        );
        routes.push(HttpRoute::exact(
            &format!("{API_ROOT}/torrents/deleteTags"),
            HttpReply::new(delete_status, Vec::new()),
        ));
        script
    };
    let assert_queue = |result| {
        let PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(items)) = result else {
            panic!("migration must not break queue feedback: {result:?}");
        };
        assert_eq!(
            items
                .iter()
                .map(|item| item.client_item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["aaaa", "bbbb"]
        );
    };
    let migration_key = format!("qbittorrent.title-tag-migration.v1:{API_ROOT}");
    for (inventory_status, delete_status, expected_batches) in
        [(200, 200, 3), (503, 200, 0), (200, 503, 1)]
    {
        let (mut store, plugin) = instantiate(
            &suite.wasm_path(),
            script_for(inventory_status, delete_status),
        );
        assert_queue(call_download_client(
            &mut store,
            &plugin,
            PluginDownloadClientCommand::ListQueue,
        ));
        let requests = &store.data().script.requests;
        let deletions = requests
            .iter()
            .filter(|request| request.url.ends_with("/torrents/deleteTags"))
            .collect::<Vec<_>>();
        assert_eq!(deletions.len(), expected_batches);
        for (index, request) in deletions.iter().enumerate() {
            assert_eq!(request.method.as_deref(), Some("POST"));
            assert_eq!(
                String::from_utf8_lossy(&request.body),
                format!(
                    "tags={}",
                    legacy.chunks(100).nth(index).unwrap().join("%2C")
                )
            );
        }
        assert!(
            requests
                .iter()
                .filter(|request| request.method.as_deref() == Some("POST"))
                .all(|request| request.url.ends_with("/auth/login")
                    || request.url.ends_with("/torrents/deleteTags"))
        );

        // Explicit state controls avoid wall-clock timing assumptions on slow runners.
        let mut state = store.data().script.stored_state();
        assert!(
            state.contains_key(&migration_key),
            "attempt is throttled even on failure"
        );
        state.insert(
            migration_key.clone(),
            serde_json::to_vec(&i64::MAX).unwrap(),
        );
        let mut throttled = script_for(200, 200);
        throttled.state = StateSource::Stored(state.clone());
        let (mut store, plugin) = instantiate(&suite.wasm_path(), throttled);
        assert_queue(call_download_client(
            &mut store,
            &plugin,
            PluginDownloadClientCommand::ListQueue,
        ));
        assert!(
            !store
                .data()
                .script
                .requests
                .iter()
                .any(|request| request.url.ends_with("/torrents/tags")
                    || request.url.ends_with("/torrents/deleteTags"))
        );

        state.insert(migration_key.clone(), serde_json::to_vec(&0i64).unwrap());
        let mut retry = script_for(200, 200);
        retry.state = StateSource::Stored(state);
        let (mut store, plugin) = instantiate(&suite.wasm_path(), retry);
        assert_queue(call_download_client(
            &mut store,
            &plugin,
            PluginDownloadClientCommand::ListQueue,
        ));
        assert_eq!(
            store
                .data()
                .script
                .requests
                .iter()
                .filter(|request| request.url.ends_with("/torrents/deleteTags"))
                .count(),
            3
        );
    }
}

fn sent_session_cookie(script: &Script) -> bool {
    script
        .requests
        .iter()
        .filter_map(|request| request.header("cookie"))
        .any(|cookie| cookie == SESSION_COOKIE)
}

/// A qBittorrent 5.2 stand-in: credentials configured, a 204 login carrying the
/// session in `Set-Cookie` and no body, and an empty torrent list.
///
/// The routes are exact: anything unrouted is a scripting bug and is refused
/// loudly rather than quietly answered with somebody else's body.
fn authenticated_script() -> Script {
    let mut config = BTreeMap::new();
    config.insert("base_url".to_string(), BASE_URL.to_string());
    config.insert("username".to_string(), "scryer".to_string());
    config.insert("password".to_string(), "secret".to_string());

    Script {
        config: ConfigSource::Resolved(config),
        // One map per `CommandHost`, shared by every invocation of a configured
        // client — which is what lets a session cookie outlive its instance.
        state: StateSource::Stored(BTreeMap::new()),
        http: HttpScript::Routed(vec![
            HttpRoute::exact(
                &format!("{API_ROOT}/auth/login"),
                // qBittorrent 5.2 answers a successful login with 204 and no
                // body; the client's `login_response_is_success` accepts that,
                // and this script would catch a regression that started
                // demanding "Ok.".
                HttpReply::new(204, Vec::new())
                    .with_header("Set-Cookie", &format!("{SESSION_COOKIE}; HttpOnly; path=/")),
            ),
            HttpRoute::exact(
                &format!("{API_ROOT}/torrents/info?sort=added_on&reverse=true&filter=all"),
                HttpReply::ok("[]"),
            ),
            HttpRoute::exact(&format!("{API_ROOT}/app/version"), HttpReply::ok("v5.2.0")),
            HttpRoute::exact(&format!("{API_ROOT}/torrents/tags"), HttpReply::ok("[]")),
        ]),
        // This client's contract distinguishes `POST /auth/login` from a `GET`
        // of the same path.
        log_http_method: true,
        ..Script::default()
    }
}
