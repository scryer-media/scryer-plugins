use super::*;
use scryer_plugin_sdk::{
    NotificationSeverity, PluginNotificationApp, PluginNotificationExternalIds,
    PluginNotificationManualInteraction, PluginNotificationTitle, PluginNotificationTitleMove,
};

/// A fixed "now" so reset arithmetic never depends on the clock.
const NOW: i64 = 1_790_000_000;
const TOKEN: &str = "o.syntheticaccesstoken";

fn settings() -> Settings {
    Settings {
        access_token: TOKEN.to_string(),
        channels: Vec::new(),
        devices: Vec::new(),
        sender: None,
        include_app_name_in_title: false,
        metadata_link: METADATA_LINK_AUTO.to_string(),
    }
}

fn with_channels(channels: &[&str]) -> Settings {
    Settings {
        channels: channels.iter().map(|value| value.to_string()).collect(),
        ..settings()
    }
}

fn with_devices(devices: &[&str]) -> Settings {
    Settings {
        devices: devices.iter().map(|value| value.to_string()).collect(),
        ..settings()
    }
}

fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
    PluginNotificationRequest {
        schema_version: 1,
        event_type,
        event_id: None,
        occurred_at: Some("2026-09-01T12:00:00+00:00".to_string()),
        correlation_id: None,
        actor: None,
        severity: Some(NotificationSeverity::Info),
        is_test: event_type == NotificationEventType::Test,
        summary_title: "Grabbed: Example Show".to_string(),
        summary_message: "Grabbed 'Example.Show.S01E01' for 'Example Show'.".to_string(),
        app: PluginNotificationApp {
            name: "Scryer".to_string(),
            version: "0.21.11".to_string(),
        },
        title: None,
        episode: None,
        episodes: Vec::new(),
        release: None,
        download: None,
        import: None,
        health: None,
        file: None,
        media_files: Vec::new(),
        application_update: None,
        manual_interaction: None,
        media_request: None,
        title_move: None,
    }
}

fn live() -> PluginNotificationRequest {
    request(NotificationEventType::Grab)
}

fn test_event() -> PluginNotificationRequest {
    request(NotificationEventType::Test)
}

fn series_title() -> PluginNotificationTitle {
    PluginNotificationTitle {
        id: Some("title-1".to_string()),
        name: "Example Show".to_string(),
        facet: "series".to_string(),
        year: Some(2024),
        slug: None,
        path: Some("/media/TV/Example Show".to_string()),
        overview: None,
        sort_title: None,
        background_url: None,
        poster_url: None,
        tags: Vec::new(),
        aliases: Vec::new(),
        original_language: None,
        original_country: None,
        external_ids: PluginNotificationExternalIds {
            tvdb_id: Some("900001".to_string()),
            ..PluginNotificationExternalIds::default()
        },
    }
}

fn reply(status: u16, body: &str) -> Result<Reply, String> {
    Ok(Reply {
        status,
        headers: BTreeMap::new(),
        body: body.as_bytes().to_vec(),
    })
}

fn reply_with_headers(status: u16, body: &str, headers: &[(&str, &str)]) -> Result<Reply, String> {
    Ok(Reply {
        status,
        headers: headers
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        body: body.as_bytes().to_vec(),
    })
}

fn pushed(iden: &str) -> Result<Reply, String> {
    reply(
        200,
        &format!(r#"{{"active":true,"iden":"{iden}","type":"note"}}"#),
    )
}

fn api_error(message: &str, param: Option<&str>) -> String {
    let mut error = json!({ "type": "invalid_request", "message": message, "cat": "~(=^‥^)ノ" });
    if let Some(param) = param {
        error["param"] = json!(param);
    }
    json!({ "error": error }).to_string()
}

/// Rejections exactly as the live API words them: an unknown or unowned target
/// is a 400 `invalid_param` naming the parameter.
const DEVICE_REJECTED: &str = "The param 'device_iden' has an invalid value.";
const CHANNEL_REJECTED: &str = "The param 'channel_tag' has an invalid value.";
const SENDER_REJECTED: &str = "The param 'source_device_iden' has an invalid value.";

/// The Pro-required error exactly as the live API has returned it. The same
/// code answers every Pro-gated limit; for a note or link push that is the
/// monthly push quota.
const QUOTA_BODY: &str = r#"{"error":{"code":"pushbullet_pro_required","type":"invalid_request","message":"Pushbullet Pro is required to make this call.","cat":"~(=^‥^)ノ"},"error_code":"pushbullet_pro_required"}"#;

/// Runs one delivery against scripted replies, returning the result and every
/// request the plugin made, in order. Running out of replies is a test failure:
/// the plugin made a request the test did not expect.
fn run(
    req: &PluginNotificationRequest,
    settings: &Settings,
    replies: Vec<Result<Reply, String>>,
) -> (PluginResult<PluginNotificationResponse>, Vec<Outbound>) {
    let mut replies = replies.into_iter();
    let mut sent = Vec::new();
    let result = deliver(
        req,
        settings,
        &mut |outbound: &Outbound| {
            sent.push(outbound.clone());
            replies.next().unwrap_or_else(|| {
                panic!("unexpected request: {} {}", outbound.method, outbound.url)
            })
        },
        Some(NOW),
    );
    (result, sent)
}

fn body_of(outbound: &Outbound) -> Value {
    serde_json::from_slice(outbound.body.as_deref().expect("a push has a body"))
        .expect("the push body is JSON")
}

fn header_of<'a>(outbound: &'a Outbound, name: &str) -> Option<&'a str> {
    outbound
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn ok(result: PluginResult<PluginNotificationResponse>) -> PluginNotificationResponse {
    match result {
        PluginResult::Ok(response) => response,
        PluginResult::Err(error) => panic!("expected an in-band response, got {error:?}"),
    }
}

fn err(result: PluginResult<PluginNotificationResponse>) -> PluginError {
    match result {
        PluginResult::Err(error) => error,
        PluginResult::Ok(response) => panic!("expected a typed error, got {response:?}"),
    }
}

fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |key| {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.to_string())
    }
}

// ---------------------------------------------------------------------------
// Descriptor and settings
// ---------------------------------------------------------------------------

#[test]
fn descriptor_keeps_every_saved_key_and_adds_the_title_and_link_options() {
    let descriptor = build_descriptor();
    let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
        panic!("pushbullet must describe a notification provider");
    };
    let by_key = |key: &str| {
        notification
            .config_fields
            .iter()
            .find(|field| field.key == key)
            .unwrap_or_else(|| panic!("{key} must remain a config field"))
    };

    assert!(by_key("api_key").required);
    assert!(matches!(
        by_key("api_key").field_type,
        ConfigFieldType::Password
    ));
    assert!(matches!(
        by_key("device_ids").field_type,
        ConfigFieldType::Tag
    ));
    assert!(matches!(
        by_key("channel_tags").field_type,
        ConfigFieldType::Tag
    ));
    assert!(matches!(
        by_key("sender_id").field_type,
        ConfigFieldType::String
    ));
    assert!(matches!(
        by_key("include_app_name_in_title").field_type,
        ConfigFieldType::Bool
    ));
    assert_eq!(
        by_key("include_app_name_in_title").default_value.as_deref(),
        Some("false")
    );
    assert_eq!(
        by_key("metadata_link").default_value.as_deref(),
        Some(METADATA_LINK_AUTO)
    );
    assert_eq!(
        notification.allowed_hosts,
        vec![PUSHBULLET_API_HOST.to_string()]
    );
}

#[test]
fn saved_lists_keep_parsing_with_every_separator_earlier_versions_accepted() {
    let settings = Settings::from_lookup(lookup(&[
        ("api_key", "  o.token  "),
        ("device_ids", "udevone;udevtwo\nudevone, 12345\r\n"),
        ("channel_tags", "alpha-channel,\nbeta-channel;alpha-channel"),
        ("sender_id", " usender "),
    ]))
    .expect("saved settings load");

    assert_eq!(settings.access_token, "o.token");
    assert_eq!(settings.devices, vec!["udevone", "udevtwo", "12345"]);
    assert_eq!(settings.channels, vec!["alpha-channel", "beta-channel"]);
    assert_eq!(settings.sender.as_deref(), Some("usender"));
    assert!(!settings.include_app_name_in_title);
    assert_eq!(settings.metadata_link, METADATA_LINK_AUTO);
}

#[test]
fn a_missing_access_token_is_a_configuration_error_naming_api_key() {
    let error = Settings::from_lookup(lookup(&[("device_ids", "udevone")]))
        .expect_err("no token, no channel");
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("api_key"), "{error:?}");
}

#[test]
fn an_unknown_metadata_link_is_a_configuration_error() {
    let error = Settings::from_lookup(lookup(&[("api_key", "t"), ("metadata_link", "nowhere")]))
        .expect_err("unknown link site");
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("metadata_link"));
}

// ---------------------------------------------------------------------------
// Request shape and routing
// ---------------------------------------------------------------------------

#[test]
fn a_push_is_a_json_body_authenticated_with_the_access_token_header() {
    let settings = Settings {
        sender: Some("usender".to_string()),
        ..settings()
    };
    let (result, sent) = run(&live(), &settings, vec![pushed("push-1")]);
    let response = ok(result);
    assert!(response.success);
    assert_eq!(response.delivery_id.as_deref(), Some("push-1"));

    let [push] = sent.as_slice() else {
        panic!("one push: {sent:?}");
    };
    assert_eq!(push.method, "POST");
    assert_eq!(push.url, PUSHES_URL);
    assert_eq!(header_of(push, "Access-Token"), Some(TOKEN));
    assert_eq!(header_of(push, "Content-Type"), Some("application/json"));
    assert_eq!(header_of(push, "Authorization"), None);

    let body = body_of(push);
    assert_eq!(body["type"], "note");
    assert_eq!(body["title"], "Grabbed: Example Show");
    assert_eq!(
        body["body"],
        "Grabbed 'Example.Show.S01E01' for 'Example Show'."
    );
    assert_eq!(body["source_device_iden"], "usender");
    // No target configured: broadcast, which Pushbullet expresses by omitting
    // every target parameter.
    for key in ["device_iden", "channel_tag", "email", "client_iden"] {
        assert!(body.get(key).is_none(), "{key} must be absent: {body}");
    }
}

#[test]
fn channels_take_priority_and_each_gets_its_own_push_and_result() {
    let mut settings = with_channels(&["alpha-channel", "beta-channel"]);
    settings.devices = vec!["udevone".to_string()];
    let (result, sent) = run(&live(), &settings, vec![pushed("push-1"), pushed("push-2")]);
    let response = ok(result);
    assert!(response.success);

    let tags: Vec<Value> = sent
        .iter()
        .map(|push| {
            let body = body_of(push);
            assert!(body.get("device_iden").is_none(), "{body}");
            body["channel_tag"].clone()
        })
        .collect();
    assert_eq!(tags, vec![json!("alpha-channel"), json!("beta-channel")]);

    let targets: Vec<(&str, bool)> = response
        .target_results
        .iter()
        .map(|result| (result.target.as_str(), result.success))
        .collect();
    assert_eq!(
        targets,
        vec![
            ("channel:alpha-channel", true),
            ("channel:beta-channel", true)
        ]
    );
}

#[test]
fn a_retried_event_repeats_each_targets_guid_so_pushbullet_does_not_duplicate_it() {
    let settings = with_devices(&["udevone", "udevtwo"]);
    let mut req = live();
    req.event_id = Some("evt-synthetic-1".to_string());
    let guids = |sent: &[Outbound]| -> Vec<String> {
        sent.iter()
            .map(|push| {
                body_of(push)["guid"]
                    .as_str()
                    .expect("an event push carries a guid")
                    .to_string()
            })
            .collect()
    };

    // The second device fails, so the core sends the whole event again.
    let (_, first) = run(&req, &settings, vec![pushed("push-1"), reply(502, "")]);
    let (_, retry) = run(&req, &settings, vec![pushed("push-1"), pushed("push-2")]);
    let first = guids(&first);
    assert_eq!(first, guids(&retry));
    assert_ne!(first[0], first[1], "each target has its own guid");

    req.event_id = Some("evt-synthetic-2".to_string());
    let (_, other) = run(&req, &settings, vec![pushed("push-3"), pushed("push-4")]);
    let other = guids(&other);
    assert!(other.iter().all(|guid| !first.contains(guid)), "{other:?}");

    req.event_id = None;
    let (_, anonymous) = run(&req, &settings, vec![pushed("push-5"), pushed("push-6")]);
    assert!(
        anonymous
            .iter()
            .all(|push| body_of(push).get("guid").is_none())
    );
}

#[test]
fn every_device_is_sent_as_a_device_iden_including_all_digit_values() {
    let settings = with_devices(&["udevone", "12345"]);
    let (result, sent) = run(&live(), &settings, vec![pushed("push-1"), pushed("push-2")]);
    assert!(ok(result).success);
    let idens: Vec<Value> = sent
        .iter()
        .map(|push| {
            let body = body_of(push);
            assert!(body.get("device_id").is_none(), "{body}");
            body["device_iden"].clone()
        })
        .collect();
    assert_eq!(idens, vec![json!("udevone"), json!("12345")]);
}

#[test]
fn the_app_name_prefix_is_opt_in_and_never_doubled() {
    let plain = settings();
    let branded = Settings {
        include_app_name_in_title: true,
        ..settings()
    };
    let req = live();
    assert_eq!(heading(&req, &plain), "Grabbed: Example Show");
    assert_eq!(heading(&req, &branded), "Scryer - Grabbed: Example Show");

    let mut untitled = live();
    untitled.summary_title = String::new();
    assert_eq!(heading(&untitled, &branded), "Scryer");
}

#[test]
fn a_title_move_renders_its_origin_destination_and_warning() {
    assert!(general_notification_events().contains(&NotificationEventType::TitleMoved));
    let mut req = request(NotificationEventType::TitleMoved);
    req.summary_title = "Moved: Example Show".to_string();
    req.summary_message = "Moved 'Example Show' from Library A to Library B.".to_string();
    req.title_move = Some(PluginNotificationTitleMove {
        source_library_name: Some("Library A".to_string()),
        destination_library_name: Some("Library B".to_string()),
        source_path: Some("/media/a/Example Show".to_string()),
        destination_path: Some("/media/b/Example Show".to_string()),
        completed_with_warnings: true,
        detail: Some("the source folder could not be removed".to_string()),
        ..PluginNotificationTitleMove::default()
    });

    let (_, sent) = run(&req, &settings(), vec![pushed("push-1")]);
    let body = body_of(&sent[0]);
    let text = body["body"].as_str().expect("the push has a body");
    assert!(text.contains("From: /media/a/Example Show"), "{text}");
    assert!(text.contains("To: /media/b/Example Show"), "{text}");
    assert!(
        text.contains("Warning: the source folder could not be removed"),
        "{text}"
    );
}

#[test]
fn a_deleted_file_path_the_summary_already_names_is_not_repeated() {
    let mut req = request(NotificationEventType::FileDeleted);
    req.summary_title = "File deleted: Example Show".to_string();
    req.summary_message =
        "Deleted media file from disk: /media/a/Example Show/S01E01.mkv".to_string();
    req.release = Some(scryer_plugin_sdk::PluginNotificationRelease {
        quality: Some("WEBDL-1080p".to_string()),
        ..Default::default()
    });
    req.file = Some(scryer_plugin_sdk::PluginNotificationFile {
        primary_path: None,
        media_updates: vec![scryer_plugin_sdk::PluginNotificationMediaUpdate {
            path: "/media/a/Example Show/S01E01.mkv".to_string(),
            update_type: scryer_plugin_sdk::NotificationMediaUpdateType::Deleted,
        }],
    });
    let (_, sent) = run(&req, &settings(), vec![pushed("push-1")]);
    let text = body_of(&sent[0])["body"]
        .as_str()
        .expect("the push has a body")
        .to_string();
    assert_eq!(
        text.matches("/media/a/Example Show/S01E01.mkv").count(),
        1,
        "{text}"
    );
    assert!(!text.contains("File:"), "{text}");
    assert!(text.contains("Quality: WEBDL-1080p"), "{text}");
}

#[test]
fn a_move_destination_the_summary_already_names_is_not_repeated() {
    let mut req = request(NotificationEventType::TitleMoved);
    req.summary_title = "Moved: Example Show".to_string();
    req.summary_message =
        "Moved 'Example Show' from Library A to Library B (/media/b/Example Show).".to_string();
    req.title_move = Some(PluginNotificationTitleMove {
        source_library_name: Some("Library A".to_string()),
        destination_library_name: Some("Library B".to_string()),
        source_path: Some("/media/a/Example Show".to_string()),
        destination_path: Some("/media/b/Example Show".to_string()),
        ..PluginNotificationTitleMove::default()
    });
    let (_, sent) = run(&req, &settings(), vec![pushed("push-1")]);
    let text = body_of(&sent[0])["body"]
        .as_str()
        .expect("the push has a body")
        .to_string();
    assert!(text.contains("From: /media/a/Example Show"), "{text}");
    assert!(!text.contains("To:"), "{text}");
    assert_eq!(text.matches("/media/b/Example Show").count(), 1, "{text}");
}

#[test]
fn a_title_with_a_metadata_id_becomes_a_link_push_unless_links_are_off() {
    let mut req = live();
    req.title = Some(series_title());

    let (_, sent) = run(&req, &settings(), vec![pushed("push-1")]);
    let body = body_of(&sent[0]);
    assert_eq!(body["type"], "link");
    assert_eq!(body["url"], "https://thetvdb.com/?tab=series&id=900001");

    let off = Settings {
        metadata_link: METADATA_LINK_NONE.to_string(),
        ..settings()
    };
    let (_, sent) = run(&req, &off, vec![pushed("push-1")]);
    let body = body_of(&sent[0]);
    assert_eq!(body["type"], "note");
    assert!(body.get("url").is_none());
}

#[test]
fn a_manual_interaction_link_wins_over_the_metadata_link() {
    let mut req = request(NotificationEventType::ManualInteractionRequired);
    req.title = Some(series_title());
    req.manual_interaction = Some(PluginNotificationManualInteraction {
        link: Some("https://scryer.example/activity/42".to_string()),
        ..PluginNotificationManualInteraction::default()
    });
    let (_, sent) = run(&req, &settings(), vec![pushed("push-1")]);
    assert_eq!(
        body_of(&sent[0])["url"],
        "https://scryer.example/activity/42"
    );
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

#[test]
fn a_rejected_token_is_auth_failed_on_api_key_and_stops_sending() {
    let settings = with_channels(&["alpha-channel", "beta-channel"]);
    let (result, sent) = run(
        &live(),
        &settings,
        vec![reply(
            401,
            &api_error("Access token is missing or invalid.", None),
        )],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
    assert!(error.public_message.contains("api_key"), "{error:?}");
    assert_eq!(sent.len(), 1, "the second channel would fail the same way");
}

#[test]
fn a_rejected_device_is_invalid_config_on_device_ids() {
    let settings = with_devices(&["ustale"]);
    let (result, _) = run(
        &live(),
        &settings,
        vec![reply(400, &api_error(DEVICE_REJECTED, Some("device_iden")))],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("device_ids"), "{error:?}");
    assert!(error.public_message.contains("ustale"), "{error:?}");
}

#[test]
fn a_refused_channel_is_invalid_config_on_channel_tags_but_a_refused_broadcast_is_the_token() {
    let (result, _) = run(
        &live(),
        &with_channels(&["borrowed-channel"]),
        vec![reply(
            403,
            &api_error("The access token is not valid for that request.", None),
        )],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("channel_tags"), "{error:?}");

    // What the live API answers for a channel this account does not own.
    let (result, _) = run(
        &live(),
        &with_channels(&["borrowed-channel"]),
        vec![reply(
            400,
            &api_error(CHANNEL_REJECTED, Some("channel_tag")),
        )],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("channel_tags"), "{error:?}");

    let (result, _) = run(
        &live(),
        &settings(),
        vec![reply(
            403,
            &api_error("The access token is not valid for that request.", None),
        )],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
    assert!(error.public_message.contains("api_key"), "{error:?}");
}

#[test]
fn a_rejected_sender_is_invalid_config_on_sender_id() {
    let settings = Settings {
        sender: Some("unot-mine".to_string()),
        ..settings()
    };
    let (result, _) = run(
        &live(),
        &settings,
        vec![reply(
            400,
            &api_error(SENDER_REJECTED, Some("source_device_iden")),
        )],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("sender_id"), "{error:?}");

    // The parameter name contains "device", so a device push must still blame
    // the sender rather than the device.
    let settings = Settings {
        sender: Some("unot-mine".to_string()),
        ..with_devices(&["udevone"])
    };
    let (result, _) = run(
        &live(),
        &settings,
        vec![reply(
            400,
            &api_error(SENDER_REJECTED, Some("source_device_iden")),
        )],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(error.public_message.contains("sender_id"), "{error:?}");
    assert!(!error.public_message.contains("device_ids"), "{error:?}");
}

#[test]
fn one_bad_device_among_good_ones_is_a_partial_failure_with_per_target_results() {
    let settings = with_devices(&["udevone", "ustale", "udevthree"]);
    let (result, sent) = run(
        &live(),
        &settings,
        vec![
            pushed("push-1"),
            reply(400, &api_error(DEVICE_REJECTED, Some("device_iden"))),
            pushed("push-3"),
        ],
    );
    assert_eq!(sent.len(), 3, "a device problem does not stop the others");
    let response = ok(result);
    assert!(!response.success);
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("device_ids") && error.contains("ustale")),
        "{response:?}"
    );
    let results: Vec<(&str, bool, Option<&str>)> = response
        .target_results
        .iter()
        .map(|result| {
            (
                result.target.as_str(),
                result.success,
                result.status.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("device:udevone", true, Some("delivered")),
            ("device:ustale", false, Some("invalid_config")),
            ("device:udevthree", true, Some("delivered")),
        ]
    );
}

#[test]
fn the_monthly_quota_is_reported_as_such_and_not_retried_across_targets() {
    // The docs do not say which status carries the quota error, so it is
    // recognised from the body whatever the status.
    for status in [400, 403, 429] {
        let settings = with_channels(&["alpha-channel", "beta-channel"]);
        let (result, sent) = run(&live(), &settings, vec![reply(status, QUOTA_BODY)]);
        let error = err(result);
        assert_eq!(error.code, PluginErrorCode::RateLimited, "HTTP {status}");
        assert!(
            error.public_message.contains("monthly push limit")
                && error.public_message.contains("500")
                && error.public_message.contains("Pushbullet Pro"),
            "{error:?}"
        );
        assert_eq!(
            sent.len(),
            1,
            "HTTP {status}: every later push would fail too"
        );
    }
}

#[test]
fn the_monthly_quota_after_a_partial_delivery_reports_what_was_delivered() {
    let settings = with_devices(&["udevone", "udevtwo", "udevthree"]);
    let (result, sent) = run(
        &live(),
        &settings,
        vec![pushed("push-1"), reply(403, QUOTA_BODY)],
    );
    assert_eq!(sent.len(), 2);
    let response = ok(result);
    assert!(!response.success);
    assert_eq!(response.provider_status.as_deref(), Some("rate_limited"));
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("monthly push limit")),
        "{response:?}"
    );
    let statuses: Vec<Option<&str>> = response
        .target_results
        .iter()
        .map(|result| result.status.as_deref())
        .collect();
    assert_eq!(
        statuses,
        vec![
            Some("delivered"),
            Some("rate_limited"),
            Some("not_attempted")
        ]
    );
}

#[test]
fn a_rate_limited_push_carries_the_delay_until_the_reset_timestamp() {
    let settings = with_devices(&["udevone", "udevtwo"]);
    let (result, sent) = run(
        &live(),
        &settings,
        vec![reply_with_headers(
            429,
            &api_error("ratelimited", None),
            &[
                ("X-Ratelimit-Limit", "16384"),
                ("X-Ratelimit-Remaining", "0"),
                ("X-Ratelimit-Reset", &(NOW + 90).to_string()),
            ],
        )],
    );
    assert_eq!(sent.len(), 1, "a rate limit stops the remaining pushes");
    let response = ok(result);
    assert!(!response.success);
    assert_eq!(response.retry_after_seconds, Some(90));
    assert_eq!(
        response.target_results[1].status.as_deref(),
        Some("not_attempted")
    );
}

#[test]
fn retry_after_wins_over_the_reset_timestamp_and_a_past_reset_is_one_second() {
    let headers = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    };
    assert_eq!(
        retry_after(
            &headers(&[("Retry-After", "30"), ("X-Ratelimit-Reset", "9999999999")]),
            Some(NOW)
        ),
        Some(30)
    );
    assert_eq!(
        retry_after(
            &headers(&[("x-ratelimit-reset", &(NOW - 5).to_string())]),
            Some(NOW)
        ),
        Some(1)
    );
    assert_eq!(retry_after(&headers(&[]), Some(NOW)), None);
}

#[test]
fn a_server_error_is_a_retryable_delivery_failure_for_that_target_only() {
    let settings = with_devices(&["udevone", "udevtwo"]);
    let (result, sent) = run(
        &live(),
        &settings,
        vec![
            reply_with_headers(503, "<html>unavailable</html>", &[("Retry-After", "20")]),
            pushed("push-2"),
        ],
    );
    assert_eq!(sent.len(), 2);
    let response = ok(result);
    assert!(!response.success);
    assert_eq!(response.retry_after_seconds, Some(20));
    assert_eq!(
        response.target_results[0].status.as_deref(),
        Some("http_503")
    );
    assert!(response.target_results[1].success);
}

#[test]
fn a_single_target_server_error_reports_the_provider_status() {
    let (result, _) = run(&live(), &settings(), vec![reply(500, "boom")]);
    let response = ok(result);
    assert!(!response.success);
    assert_eq!(response.provider_status.as_deref(), Some("http_500"));
}

#[test]
fn an_unrecognised_rejection_is_a_permanent_error_about_the_push_itself() {
    let (result, _) = run(
        &live(),
        &settings(),
        vec![reply(400, &api_error("Invalid type.", Some("type")))],
    );
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::Permanent);
    assert!(error.public_message.contains("Invalid type."), "{error:?}");
}

#[test]
fn an_unreachable_host_is_a_delivery_failure_live_and_a_typed_error_on_test() {
    let settings = with_channels(&["alpha-channel", "beta-channel"]);
    let (result, sent) = run(&live(), &settings, vec![Err("egress refused".to_string())]);
    assert_eq!(sent.len(), 1);
    let response = ok(result);
    assert!(!response.success);
    assert_eq!(
        response.target_results[0].status.as_deref(),
        Some("request_failed")
    );

    let (result, _) = run(
        &test_event(),
        &settings,
        vec![Err("egress refused".to_string())],
    );
    assert_eq!(err(result).code, PluginErrorCode::UpstreamUnavailable);
}

#[test]
fn a_low_request_budget_is_a_warning_on_a_successful_push() {
    let (result, _) = run(
        &live(),
        &settings(),
        vec![reply_with_headers(
            200,
            r#"{"iden":"push-1"}"#,
            &[
                ("X-Ratelimit-Limit", "16384"),
                ("X-Ratelimit-Remaining", "100"),
            ],
        )],
    );
    let response = ok(result);
    assert!(response.success);
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.contains("rate limit")),
        "{response:?}"
    );
}

// ---------------------------------------------------------------------------
// Test-time verification
// ---------------------------------------------------------------------------

const DEVICE_LIST: &str = r#"{"devices":[
    {"iden":"udevone","nickname":"Phone","active":true},
    {"iden":"udevtwo","nickname":"tablet","active":true},
    {"iden":"udeleted","nickname":"Old Phone","active":false}
]}"#;

#[test]
fn a_test_names_unknown_devices_and_lists_the_real_ones_before_pushing() {
    let settings = with_devices(&["udevone", "phone", "12345"]);
    let (result, sent) = run(&test_event(), &settings, vec![reply(200, DEVICE_LIST)]);
    assert_eq!(
        sent.len(),
        1,
        "no push is sent to a misconfigured device list"
    );
    assert_eq!(sent[0].method, "GET");
    assert!(sent[0].url.starts_with(DEVICES_URL), "{}", sent[0].url);
    assert_eq!(header_of(&sent[0], "Access-Token"), Some(TOKEN));

    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    let message = &error.public_message;
    assert!(message.starts_with("device_ids"), "{message}");
    assert!(message.contains("use its iden \"udevone\""), "{message}");
    assert!(
        message.contains("\"12345\" is not an active device"),
        "{message}"
    );
    assert!(message.contains("tablet (udevtwo)"), "{message}");
    assert!(!message.contains("udeleted"), "{message}");
}

#[test]
fn a_test_with_known_devices_goes_on_to_push() {
    let settings = with_devices(&["udevtwo"]);
    let (result, sent) = run(
        &test_event(),
        &settings,
        vec![reply(200, DEVICE_LIST), pushed("push-1")],
    );
    assert!(ok(result).success);
    assert_eq!(sent.len(), 2);
    assert_eq!(body_of(&sent[1])["device_iden"], "udevtwo");
}

#[test]
fn a_test_follows_the_device_list_cursor() {
    let settings = with_devices(&["ulater"]);
    let (result, sent) = run(
        &test_event(),
        &settings,
        vec![
            reply(
                200,
                r#"{"devices":[{"iden":"udevone","active":true}],"cursor":"page 2"}"#,
            ),
            reply(200, r#"{"devices":[{"iden":"ulater","active":true}]}"#),
            pushed("push-1"),
        ],
    );
    assert!(ok(result).success);
    assert!(sent[1].url.contains("cursor=page%202"), "{}", sent[1].url);
}

#[test]
fn a_test_whose_device_list_is_refused_for_the_token_is_auth_failed() {
    let settings = with_devices(&["udevone"]);
    let (result, sent) = run(
        &test_event(),
        &settings,
        vec![reply(
            401,
            &api_error("Access token is missing or invalid.", None),
        )],
    );
    assert_eq!(sent.len(), 1);
    let error = err(result);
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
    assert!(error.public_message.contains("api_key"));
}

#[test]
fn a_test_that_cannot_list_devices_warns_and_still_pushes() {
    let settings = with_devices(&["udevone"]);
    let (result, sent) = run(
        &test_event(),
        &settings,
        vec![reply(502, "bad gateway"), pushed("push-1")],
    );
    let response = ok(result);
    assert!(response.success);
    assert_eq!(sent.len(), 2);
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.contains("could not list")),
        "{response:?}"
    );
}

#[test]
fn a_test_warns_about_ignored_devices_and_an_unknown_sender() {
    let settings = Settings {
        channels: vec!["alpha-channel".to_string()],
        devices: vec!["udevone".to_string()],
        sender: Some("unot-mine".to_string()),
        ..settings()
    };
    let (result, _) = run(
        &test_event(),
        &settings,
        vec![reply(200, DEVICE_LIST), pushed("push-1")],
    );
    let response = ok(result);
    assert!(response.success);
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.contains("device_ids is ignored")),
        "{response:?}"
    );
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.contains("sender_id")),
        "{response:?}"
    );
}

#[test]
fn a_live_send_does_not_list_devices() {
    let settings = with_devices(&["udevone"]);
    let (result, sent) = run(&live(), &settings, vec![pushed("push-1")]);
    assert!(ok(result).success);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].url, PUSHES_URL);
}

// ---------------------------------------------------------------------------
// Device list action
// ---------------------------------------------------------------------------

fn options(
    token: Option<&str>,
    replies: Vec<Result<Reply, String>>,
) -> PluginResult<PluginActionResponse> {
    let mut replies = replies.into_iter();
    device_options(token.map(str::to_string), &mut |outbound: &Outbound| {
        replies
            .next()
            .unwrap_or_else(|| panic!("unexpected request: {}", outbound.url))
    })
}

#[test]
fn the_device_list_action_offers_active_named_devices_sorted_by_name() {
    let result = options(
        Some(TOKEN),
        vec![reply(
            200,
            r#"{"devices":[
                {"iden":"udevtwo","nickname":"tablet","active":true},
                {"iden":"udevone","nickname":"Phone","active":true},
                {"iden":"unamed","active":true},
                {"iden":"udeleted","nickname":"Old Phone","active":false}
            ]}"#,
        )],
    );
    let PluginResult::Ok(response) = result else {
        panic!("{result:?}");
    };
    assert_eq!(
        response.payload,
        json!({ "options": [
            { "id": "udevone", "name": "Phone" },
            { "id": "udevtwo", "name": "tablet" },
        ]})
    );
}

#[test]
fn the_device_list_action_without_a_token_offers_nothing_and_calls_nothing() {
    let PluginResult::Ok(response) = options(None, Vec::new()) else {
        panic!("no token is an empty list, not an error");
    };
    assert_eq!(response.payload, json!({ "options": [] }));
}

#[test]
fn the_device_list_action_reports_a_rejected_token_as_auth_failed() {
    let result = options(
        Some(TOKEN),
        vec![reply(
            401,
            &api_error("Access token is missing or invalid.", None),
        )],
    );
    let PluginResult::Err(error) = result else {
        panic!("{result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
}
