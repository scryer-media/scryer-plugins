//! Conformance against the real Scryer download-client host, run on the
//! RELEASE artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way the host does.
//! What stays here is what is genuinely this plugin's: its descriptor identity
//! and its artifact name.

use scryer_plugin_conformance::download_client::DownloadClientConformance;

#[test]
fn transmission_release_wasm_conforms_to_the_download_client_host_contract() {
    DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "transmission")
        .wasm("transmission_download_client.wasm")
        .run();
}

#[test]
fn session_versions_and_handshake_work_through_the_component() {
    use scryer_plugin_conformance::download_client::{call_download_client, instantiate_with};
    use scryer_plugin_conformance::{
        ConfigSource, HostErrorKind, HostResponder, HttpReply, HttpRoute, HttpScript, Script,
        StateSource, default_respond,
    };
    use scryer_plugin_sdk::PluginResult;
    use scryer_plugin_sdk::command::{
        PluginDownloadClientCommand as Command, PluginDownloadClientCommandResult as Result,
    };
    use scryer_plugin_sdk::host::{PluginHostRequest, PluginHostResponse};

    struct RpcHost {
        body: String,
    }
    impl HostResponder for RpcHost {
        fn respond(
            &mut self,
            request: PluginHostRequest,
            script: &mut Script,
        ) -> std::result::Result<PluginHostResponse, HostErrorKind> {
            if matches!(&request, PluginHostRequest::Http(_)) {
                let reply = if script.requests.is_empty() {
                    HttpReply::new(409, vec![])
                        .with_header("X-Transmission-Session-Id", "synthetic-session")
                } else {
                    HttpReply::ok(&self.body)
                };
                script.http = HttpScript::Routed(vec![HttpRoute::any(reply)]);
            }
            default_respond(request, script)
        }
    }
    let suite = DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "transmission")
        .wasm("transmission_download_client.wasm");
    for (protocol, version) in [
        (serde_json::json!(19), Some("4.1.3 (synthetic)")),
        (serde_json::json!("19"), Some("4.1.3 (synthetic)")),
        (serde_json::json!("14"), None),
    ] {
        let body = serde_json::json!({"result":"success","arguments":{
            "rpc-version":protocol,"version":version,"download-dir":"/downloads","torrents":[]
        }})
        .to_string();
        let script = Script {
            config: ConfigSource::Resolved(
                [("host".into(), "transmission.test.invalid".into())].into(),
            ),
            state: StateSource::Stored(Default::default()),
            ..Default::default()
        };
        let (mut store, plugin) = instantiate_with(&suite.wasm_path(), script, RpcHost { body });
        for command in [
            Command::TestConnection,
            Command::Status,
            Command::ListQueue,
            Command::ListHistory,
        ] {
            let result = call_download_client(&mut store, &plugin, command);
            assert!(
                matches!(
                    result,
                    Result::TestConnection(PluginResult::Ok(_))
                        | Result::Status(PluginResult::Ok(_))
                        | Result::ListQueue(PluginResult::Ok(_))
                        | Result::ListHistory(PluginResult::Ok(_))
                ),
                "{result:?}"
            );
        }
        let requests = &store.data().script.requests;
        assert!(requests.len() >= 5);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(
            requests[1].header("X-Transmission-Session-Id"),
            Some("synthetic-session")
        );
        assert!(requests[1..].iter().all(
            |request| request.header("X-Transmission-Session-Id") == Some("synthetic-session")
        ));
    }
}

/// Transmission answers wrong RPC credentials with 401; the operator must see
/// that the username or password was refused, not a retried generic failure.
#[test]
fn rejected_rpc_credentials_report_their_code_through_the_component() {
    use scryer_plugin_conformance::download_client::{call_download_client, instantiate};
    use scryer_plugin_conformance::{
        ConfigSource, HttpReply, HttpRoute, HttpScript, Script, StateSource,
    };
    use scryer_plugin_sdk::command::{
        PluginDownloadClientCommand as Command, PluginDownloadClientCommandResult as Result,
    };
    use scryer_plugin_sdk::{PluginErrorCode, PluginResult};

    let suite = DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "transmission")
        .wasm("transmission_download_client.wasm");
    let script = Script {
        config: ConfigSource::Resolved(
            [
                ("host".into(), "transmission.test.invalid".into()),
                ("username".into(), "synthetic-user".into()),
                ("password".into(), "synthetic-password".into()),
            ]
            .into(),
        ),
        state: StateSource::Stored(Default::default()),
        http: HttpScript::Routed(vec![HttpRoute::any(HttpReply::new(
            401,
            b"<h1>401: Unauthorized</h1>".to_vec(),
        ))]),
        ..Default::default()
    };
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script);
    let result = call_download_client(&mut store, &plugin, Command::TestConnection);
    let Result::TestConnection(PluginResult::Err(error)) = result else {
        panic!("rejected credentials must be a typed plugin error: {result:?}");
    };
    assert_eq!(
        error.code,
        PluginErrorCode::from("invalid_username_password"),
        "{error:?}"
    );
    assert!(!error.public_message.contains("synthetic-password"));
}

/// The host routes a default grab to its facet (`series`) while the plugin's own
/// category stays `scryer-tv`. The torrent gets that one label, and both the
/// unscoped listing and a `series`-scoped listing still return it.
#[test]
fn a_routed_grab_carries_one_label_and_stays_listed() {
    use scryer_plugin_conformance::download_client::{call_download_client, instantiate_with};
    use scryer_plugin_conformance::{
        ConfigSource, HostErrorKind, HostResponder, HttpReply, HttpRoute, HttpScript, Script,
        StateSource, default_respond,
    };
    use scryer_plugin_sdk::command::{
        PluginDownloadClientCommand as Command, PluginDownloadClientCommandResult as Result,
    };
    use scryer_plugin_sdk::host::{PluginHostRequest, PluginHostResponse};
    use scryer_plugin_sdk::{
        PluginDownloadClientAddRequest, PluginDownloadFeedbackScope,
        PluginDownloadScopedListRequest, PluginResult,
    };

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A Transmission 4.x stand-in that answers by RPC method and remembers the
    /// labels `torrent-add` asked for.
    #[derive(Default)]
    struct RpcHost {
        handshaken: bool,
        labels: Option<serde_json::Value>,
    }
    impl HostResponder for RpcHost {
        fn respond(
            &mut self,
            request: PluginHostRequest,
            script: &mut Script,
        ) -> std::result::Result<PluginHostResponse, HostErrorKind> {
            if let PluginHostRequest::Http(http) = &request {
                let reply = if !self.handshaken {
                    self.handshaken = true;
                    HttpReply::new(409, vec![])
                        .with_header("X-Transmission-Session-Id", "synthetic-session")
                } else {
                    let rpc: serde_json::Value =
                        serde_json::from_slice(&http.body).unwrap_or_default();
                    let arguments = match rpc["method"].as_str() {
                        Some("session-get") => serde_json::json!({
                            "rpc-version": 19, "version": "4.1.3 (synthetic)",
                            "download-dir": "/downloads"
                        }),
                        Some("torrent-add") => {
                            self.labels = rpc["arguments"].get("labels").cloned();
                            serde_json::json!({"torrent-added": {"id": 1, "hashString": HASH}})
                        }
                        Some("torrent-get") => serde_json::json!({"torrents": [{
                            "id": 1, "hashString": HASH, "name": "Synthetic.Series.S01E01",
                            "downloadDir": "/downloads/scryer-tv", "totalSize": 100,
                            "leftUntilDone": 50, "status": 4,
                            "labels": self.labels.clone().unwrap_or_default()
                        }]}),
                        _ => serde_json::json!({}),
                    };
                    HttpReply::ok(
                        &serde_json::json!({"result": "success", "arguments": arguments})
                            .to_string(),
                    )
                };
                script.http = HttpScript::Routed(vec![HttpRoute::any(reply)]);
            }
            default_respond(request, script)
        }
    }

    let suite = DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "transmission")
        .wasm("transmission_download_client.wasm");
    let script = Script {
        config: ConfigSource::Resolved(
            [("host".into(), "transmission.test.invalid".into())].into(),
        ),
        state: StateSource::Stored(Default::default()),
        ..Default::default()
    };
    let (mut store, plugin) = instantiate_with(&suite.wasm_path(), script, RpcHost::default());

    let add: PluginDownloadClientAddRequest = serde_json::from_value(serde_json::json!({
        "source": {"kind": "magnet_uri", "magnet_uri": format!("magnet:?xt=urn:btih:{HASH}")},
        "release": {"release_title": "Synthetic.Series.S01E01"},
        "title": {"title_name": "Synthetic Series", "media_facet": "series", "tags": []},
        "routing": {"isolation_value": "series"}
    }))
    .unwrap();
    let result = call_download_client(&mut store, &plugin, Command::Add(add));
    assert!(
        matches!(result, Result::Add(PluginResult::Ok(_))),
        "{result:?}"
    );
    let add_body = store
        .data()
        .script
        .requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .find(|rpc| rpc["method"] == "torrent-add")
        .expect("torrent-add was sent");
    assert_eq!(
        add_body["arguments"]["labels"],
        serde_json::json!(["series"])
    );

    let listed = |result: Result| -> Vec<String> {
        match result {
            Result::ListQueue(PluginResult::Ok(items)) => items,
            Result::ListQueueScoped(PluginResult::Ok(response)) => response.items,
            other => panic!("listing failed: {other:?}"),
        }
        .into_iter()
        .map(|item| item.client_item_id)
        .collect()
    };
    let scoped = |categories: &[&str]| {
        Command::ListQueueScoped(PluginDownloadScopedListRequest {
            scope: PluginDownloadFeedbackScope {
                categories: categories.iter().map(|value| value.to_string()).collect(),
            },
        })
    };
    assert_eq!(
        listed(call_download_client(
            &mut store,
            &plugin,
            Command::ListQueue
        )),
        vec![HASH]
    );
    assert_eq!(
        listed(call_download_client(
            &mut store,
            &plugin,
            scoped(&["series"])
        )),
        vec![HASH]
    );
    assert!(
        listed(call_download_client(
            &mut store,
            &plugin,
            scoped(&["movie"])
        ))
        .is_empty()
    );
}
