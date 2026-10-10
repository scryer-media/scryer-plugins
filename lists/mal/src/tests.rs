use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListMediaKind, ListPluginAccountRequest, ListPluginFetchRequest, ListPluginHealthRequest,
    PluginDescriptor, PluginErrorCode, PluginResult,
};

use super::*;

const TOKEN: &str = "fixture-member-access-token-0123456789";

fn list_url(status: &str, offset: u32) -> String {
    format!(
        "https://api.myanimelist.net/v2/users/@me/animelist?status={status}&sort=anime_title\
         &fields=media_type,start_date,start_season,status,mean,genres&limit=1000&offset={offset}&nsfw=true"
    )
}

const ACCOUNT_URL: &str = "https://api.myanimelist.net/v2/users/@me";

fn credential(token: &str) -> ListCredential {
    ListCredential {
        access_token: token.to_string(),
        token_type: Some("Bearer".to_string()),
        external_user_id: Some("990001".to_string()),
        username: Some("fixture-member".to_string()),
    }
}

fn request(source_type: &str, cursor: Option<&str>) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: source_type.to_string(),
        params: BTreeMap::new(),
        credential: Some(credential(TOKEN)),
        page_cursor: cursor.map(str::to_string),
        since_fingerprint: None,
    }
}

fn fetch(
    http: &RecordedHttp,
    request: ListPluginFetchRequest,
) -> PluginResult<scryer_plugin_sdk::ListPluginFetchResponse> {
    match block_on(run(http, PluginListCommand::Fetch(request))) {
        PluginListCommandResult::Fetch(result) => result,
        other => panic!("unexpected {other:?}"),
    }
}

fn account_of(
    http: &RecordedHttp,
    token: &str,
) -> PluginResult<scryer_plugin_sdk::ListPluginAccountResponse> {
    match block_on(run(
        http,
        PluginListCommand::Account(ListPluginAccountRequest {
            credential: credential(token),
        }),
    )) {
        PluginListCommandResult::Account(result) => result,
        other => panic!("unexpected {other:?}"),
    }
}

fn ok<T: std::fmt::Debug>(result: PluginResult<T>) -> T {
    match result {
        PluginResult::Ok(value) => value,
        PluginResult::Err(error) => panic!("unexpected error {error:?}"),
    }
}

fn err<T: std::fmt::Debug>(result: PluginResult<T>) -> scryer_plugin_sdk::PluginError {
    match result {
        PluginResult::Err(error) => error,
        PluginResult::Ok(value) => panic!("unexpected success {value:?}"),
    }
}

fn entry(id: u32, title: &str, media_type: &str) -> String {
    format!(
        r#"{{"node": {{"id": {id}, "title": "{title}",
            "main_picture": {{"medium": "https://cdn.example.test/anime/{id}.jpg",
                              "large": "https://cdn.example.test/anime/{id}l.jpg"}},
            "media_type": "{media_type}", "start_date": "2031-04-05",
            "start_season": {{"year": 2031, "season": "spring"}}, "status": "finished_airing"}}}}"#
    )
}

/// A full page of `count` entries whose ids start at `first_id`.
fn entries(first_id: u32, count: u32) -> Vec<String> {
    (first_id..first_id + count)
        .map(|id| entry(id, "Fixture Paged Title", "tv"))
        .collect()
}

fn page(entries: &[String], next: Option<&str>) -> String {
    let paging = match next {
        Some(next) => format!(r#"{{"next": "{next}"}}"#),
        None => "{}".to_string(),
    };
    format!(r#"{{"data": [{}], "paging": {paging}}}"#, entries.join(","))
}

/// One entry per media type MyAnimeList documents, plus the edge cases.
const MIXED_PAGE: &str = r#"{
  "data": [
    {"node": {"id": 990101, "title": "Fixture Serial Alpha",
      "main_picture": {"medium": "https://cdn.example.test/a.jpg", "large": "https://cdn.example.test/al.jpg"},
      "media_type": "tv", "start_date": "2030-10-02", "start_season": {"year": 2030, "season": "fall"},
      "status": "currently_airing", "mean": 7.85,
      "genres": [{"id": 1, "name": "Action"}, {"id": 24, "name": "Sci-Fi"}]},
     "list_status": {"status": "plan_to_watch", "score": 9, "num_episodes_watched": 3,
      "is_rewatching": false, "updated_at": "2031-01-01T00:00:00+00:00", "comments": "fixture note"}},
    {"node": {"id": 990102, "title": "Fixture Feature Beta", "media_type": "movie",
      "start_date": "2032", "status": "not_yet_aired"}},
    {"node": {"id": 990103, "title": "Fixture Video Gamma", "media_type": "ova",
      "start_season": {"year": 2029, "season": "winter"}, "mean": 0}},
    {"node": {"id": 990104, "title": "Fixture Net Delta", "media_type": "ona", "start_date": "2028-07"}},
    {"node": {"id": 990105, "title": "Fixture Extra Epsilon", "media_type": "special"}},
    {"node": {"id": 990106, "title": "Fixture Broadcast Zeta", "media_type": "tv_special"}},
    {"node": {"id": 990107, "title": "Fixture Announced Eta", "media_type": "unknown"}},
    {"node": {"id": 990108, "title": "Fixture Untyped Theta"}},
    {"node": {"id": 990109, "title": "Fixture Song Iota", "media_type": "music"}},
    {"node": {"id": 990110, "title": "Fixture Advert Kappa", "media_type": "cm"}},
    {"node": {"id": 990111, "title": "Fixture Promo Lambda", "media_type": "pv"}},
    {"node": {"title": "Fixture Missing Id"}},
    {"node": {"id": 990101, "title": "Fixture Serial Alpha", "media_type": "tv"}}
  ],
  "paging": {}
}"#;

#[test]
fn descriptor_round_trips_and_passes_host_checks() {
    let original = descriptor();
    let decoded: PluginDescriptor =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    scryer_plugin_sdk::validate_plugin_descriptor_sdk_contract(
        &decoded,
        scryer_plugin_sdk::SDK_VERSION,
    )
    .unwrap();
    scryer_plugin_sdk::validate_plugin_descriptor_host_permissions(&decoded).unwrap();
    assert_eq!(decoded.id, "mal-list");

    let list = decoded.list_provider().unwrap();
    assert_eq!(list.provider_type, "mal");
    assert_eq!(
        list.auth,
        ListProviderAuth::MemberAccount {
            flow: ListAccountFlow::AuthorizationCode { pkce: true },
            exchange: ListAccountExchange::SmgRelay,
            byo_app: true,
            scopes: vec!["write:users".to_string()],
        }
    );
    assert_eq!(
        list.capabilities,
        ListProviderCapabilities {
            account: true,
            health: false,
            requires_member_credential: true,
        }
    );
    assert!(list.config_fields.is_empty(), "no server key or client id");
    assert!(list.url_patterns.is_empty(), "nothing public to recognise");
    assert_eq!(list.allowed_hosts, vec!["api.myanimelist.net".to_string()]);
    assert_eq!(list.rate_limit_seconds, Some(2));
    assert_eq!(
        list.coverage,
        vec![ListMediaKind::Anime, ListMediaKind::Movie]
    );

    assert_eq!(list.groups.len(), 1);
    assert_eq!(list.groups[0].auth_badge, ListAuthBadge::MemberAccount);
    let items = &list.groups[0].items;
    let sources: Vec<_> = items.iter().map(|item| item.source_type.as_str()).collect();
    assert_eq!(
        sources,
        vec![
            "status:watching",
            "status:completed",
            "status:on_hold",
            "status:dropped",
            "status:plan_to_watch",
        ]
    );
    for item in items {
        assert!(item.personal, "{} is a member's own list", item.id);
        assert!(item.params.is_empty());
        assert_eq!(item.kinds, vec![ListMediaKind::Anime, ListMediaKind::Movie]);
        assert_eq!(item.default_interval_seconds, 6 * 60 * 60);
    }
}

#[test]
fn every_status_reads_its_own_list_with_the_member_token() {
    for (status, label) in STATUSES {
        let http = RecordedHttp::new().with(
            &list_url(status, 0),
            200,
            &page(&[entry(990201, "Fixture Serial Mu", "tv")], None),
        );
        let response = ok(fetch(&http, request(&format!("status:{status}"), None)));
        assert_eq!(response.items.len(), 1, "{status}");
        assert_eq!(
            response.list_name.as_deref(),
            Some(format!("MyAnimeList {label}").as_str())
        );
        assert_eq!(response.list_url, None, "no username in a stored address");

        let sent = &http.requests()[0];
        assert_eq!(sent.method.as_deref(), Some("GET"));
        assert!(!sent.url.contains(TOKEN));
        assert_eq!(
            sent.headers.get("Authorization").map(String::as_str),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert!(
            !sent
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("X-MAL-CLIENT-ID")),
            "member calls need no client id"
        );
        assert!(
            !sent.url.contains("list_status"),
            "the member's scores and progress are never requested"
        );
    }
}

#[test]
fn media_types_map_to_kinds_and_formats() {
    let http = RecordedHttp::new().with(&list_url("plan_to_watch", 0), 200, MIXED_PAGE);
    let response = ok(fetch(&http, request("status:plan_to_watch", None)));
    let rows: Vec<_> = response
        .items
        .iter()
        .map(|item| {
            (
                item.item_key.as_str(),
                item.kind_hint,
                item.format.as_deref(),
                item.rank,
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "mal:990101",
                Some(ListMediaKind::Anime),
                Some("tv"),
                Some(1)
            ),
            (
                "mal:990102",
                Some(ListMediaKind::Movie),
                Some("movie"),
                Some(2)
            ),
            (
                "mal:990103",
                Some(ListMediaKind::Anime),
                Some("ova"),
                Some(3)
            ),
            (
                "mal:990104",
                Some(ListMediaKind::Anime),
                Some("ona"),
                Some(4)
            ),
            (
                "mal:990105",
                Some(ListMediaKind::Anime),
                Some("special"),
                Some(5)
            ),
            (
                "mal:990106",
                Some(ListMediaKind::Anime),
                Some("tv_special"),
                Some(6)
            ),
            ("mal:990107", Some(ListMediaKind::Anime), None, Some(7)),
            ("mal:990108", Some(ListMediaKind::Anime), None, Some(8)),
        ],
        "music, commercials, promos, the id-less entry and the duplicate are dropped"
    );

    let series = &response.items[0];
    assert_eq!(
        series.external_ids,
        vec![ListExternalId {
            source: "mal".to_string(),
            kind: Some("anime".to_string()),
            id: "990101".to_string(),
        }]
    );
    assert_eq!(series.title, None, "the id resolves the entry");
    assert_eq!(series.year, Some(2030));
    assert_eq!(series.released, Some(true));
    assert_eq!(
        series
            .provider_rating
            .as_ref()
            .map(|rating| (rating.scale.as_str(), rating.value)),
        Some(("mal", 7.85))
    );
    assert_eq!(
        series.genres,
        vec!["Action".to_string(), "Sci-Fi".to_string()]
    );

    let film = &response.items[1];
    assert_eq!(film.external_ids[0].kind.as_deref(), Some("anime"));
    assert_eq!(film.year, Some(2032));
    assert_eq!(film.released, Some(false));

    let video = &response.items[2];
    assert_eq!(
        video.year,
        Some(2029),
        "the start season fills a missing date"
    );
    assert!(
        video.provider_rating.is_none(),
        "an unscored title has no rating"
    );
    assert_eq!(video.released, None);
    assert_eq!(response.items[3].year, Some(2028));

    assert_eq!(response.total_hint, Some(8));
    assert!(response.next_cursor.is_none());
    assert!(response.fingerprint.is_some());
    let serialized = serde_json::to_string(&response).unwrap();
    for private in [
        "fixture note",
        "Fixture Serial Alpha",
        "num_episodes_watched",
    ] {
        assert!(!serialized.contains(private), "{private} is not passed on");
    }
}

#[test]
fn a_single_page_list_is_fingerprinted() {
    let http = RecordedHttp::new().with(&list_url("completed", 0), 200, MIXED_PAGE);
    let first = ok(fetch(&http, request("status:completed", None)));
    let mut again = request("status:completed", None);
    again.since_fingerprint = first.fingerprint.clone();
    let unchanged = ok(fetch(&http, again));
    assert!(unchanged.unchanged);
    assert!(unchanged.items.is_empty());
    assert_eq!(unchanged.fingerprint, first.fingerprint);
}

#[test]
fn pages_follow_paging_next_with_global_ranks() {
    let first_page = page(
        &entries(1_000_000, PAGE_LIMIT),
        Some(
            "https://api.myanimelist.net/v2/users/@me/animelist?offset=1000&status=watching&limit=1000",
        ),
    );
    // MyAnimeList's next address is never followed as given: only its offset
    // is read, so a stray host or parameter cannot leave the API.
    let second_page = page(
        &entries(1_001_000, PAGE_LIMIT),
        Some("https://elsewhere.example.test/v2/users/@me/animelist?limit=5&offset=2000"),
    );
    let last_page = page(&[entry(990304, "Fixture Paged Four", "tv")], None);
    let http = RecordedHttp::new()
        .with(&list_url("watching", 0), 200, &first_page)
        .with(&list_url("watching", 1000), 200, &second_page)
        .with(&list_url("watching", 2000), 200, &last_page);

    let first = ok(fetch(&http, request("status:watching", None)));
    assert_eq!(first.next_cursor.as_deref(), Some("1000"));
    assert!(
        first.fingerprint.is_none(),
        "a paged list has no fingerprint"
    );
    assert_eq!(first.total_hint, None);
    assert_eq!(first.items.len(), 1000);
    assert_eq!(first.items[0].rank, Some(1));
    assert_eq!(first.items[999].rank, Some(1000));

    let second = ok(fetch(
        &http,
        request("status:watching", first.next_cursor.as_deref()),
    ));
    assert_eq!(second.next_cursor.as_deref(), Some("2000"));
    assert_eq!(second.items[0].rank, Some(1001));

    let mut last_request = request("status:watching", second.next_cursor.as_deref());
    last_request.since_fingerprint = Some("v1:stale".to_string());
    let last = ok(fetch(&http, last_request));
    assert!(last.next_cursor.is_none());
    assert!(!last.unchanged);
    assert!(last.fingerprint.is_none());
    assert_eq!(last.items[0].rank, Some(2001));

    assert_eq!(
        http.urls(),
        vec![
            list_url("watching", 0),
            list_url("watching", 1000),
            list_url("watching", 2000),
        ]
    );
}

#[test]
fn a_short_page_continues_right_after_its_last_entry() {
    let first_page = page(
        &entries(990401, 3),
        Some("https://api.myanimelist.net/v2/users/@me/animelist?offset=3"),
    );
    let http = RecordedHttp::new().with(&list_url("dropped", 0), 200, &first_page);
    let first = ok(fetch(&http, request("status:dropped", None)));
    assert_eq!(first.next_cursor.as_deref(), Some("3"));
}

#[test]
fn a_next_page_that_does_not_follow_this_one_fails() {
    // An offset that stays put, goes back, skips entries or is missing would
    // repeat or drop titles, so the sync fails instead of guessing.
    for next in [
        "https://api.myanimelist.net/v2/users/@me/animelist?offset=0",
        "https://api.myanimelist.net/v2/users/@me/animelist?offset=1000",
        "https://api.myanimelist.net/v2/users/@me/animelist?offset=5000",
        "https://api.myanimelist.net/v2/users/@me/animelist?limit=1000",
        "https://api.myanimelist.net/v2/users/@me/animelist?offset=fixture",
    ] {
        let http = RecordedHttp::new().with(
            &list_url("dropped", 0),
            200,
            &page(&entries(990451, 3), Some(next)),
        );
        let error = err(fetch(&http, request("status:dropped", None)));
        assert_eq!(error.code, PluginErrorCode::Permanent, "{next}");
        assert!(!error.public_message.contains("not found"), "{next}");
    }

    // A jump past the page just read, from a later page.
    let http = RecordedHttp::new().with(
        &list_url("dropped", 1000),
        200,
        &page(
            &entries(990461, PAGE_LIMIT),
            Some("https://api.myanimelist.net/v2/users/@me/animelist?offset=3000"),
        ),
    );
    let error = err(fetch(&http, request("status:dropped", Some("1000"))));
    assert_eq!(error.code, PluginErrorCode::Permanent);
}

#[test]
fn lists_beyond_the_page_cap_fail_instead_of_being_cut_short() {
    let last_followed = (MAX_PAGES - 1) * PAGE_LIMIT;
    const { assert!(MAX_PAGES < 100, "below the host's hundred-page ceiling") };
    let http = RecordedHttp::new().with(
        &list_url("completed", last_followed),
        200,
        &page(
            &entries(990501, PAGE_LIMIT),
            Some(&format!(
                "https://api.myanimelist.net/v2/users/@me/animelist?offset={}",
                last_followed + PAGE_LIMIT
            )),
        ),
    );
    let error = err(fetch(
        &http,
        request("status:completed", Some(&last_followed.to_string())),
    ));
    assert_eq!(error.code, PluginErrorCode::Permanent);
    assert!(error.public_message.contains("20000"));
    assert!(
        !error.public_message.contains("not found"),
        "the list is not gone"
    );

    let beyond = err(fetch(
        &http,
        request(
            "status:completed",
            Some(&(MAX_PAGES * PAGE_LIMIT).to_string()),
        ),
    ));
    assert_eq!(beyond.code, PluginErrorCode::Permanent);
    assert_eq!(http.urls().len(), 1, "a cursor past the cap is never sent");
}

#[test]
fn a_fetch_without_a_member_token_is_refused() {
    let http = RecordedHttp::new();
    let mut anonymous = request("status:plan_to_watch", None);
    anonymous.credential = None;
    let missing = err(fetch(&http, anonymous));
    assert_eq!(missing.code, PluginErrorCode::AuthFailed);

    let mut blank = request("status:plan_to_watch", None);
    blank.credential = Some(credential("   "));
    assert_eq!(err(fetch(&http, blank)).code, PluginErrorCode::AuthFailed);

    let broken_token = "fixture\r\nX-Injected: 1";
    let mut broken = request("status:plan_to_watch", None);
    broken.credential = Some(credential(broken_token));
    let refused = err(fetch(&http, broken));
    assert_eq!(refused.code, PluginErrorCode::AuthFailed);
    assert!(!refused.public_message.contains("fixture"));

    assert!(http.urls().is_empty(), "nothing is sent without a token");
}

#[test]
fn errors_map_to_host_failure_classes() {
    let list = |status: u16, headers: &[(&str, &str)], body: &str| {
        let http =
            RecordedHttp::new().with_headers(&list_url("plan_to_watch", 0), status, headers, body);
        err(fetch(&http, request("status:plan_to_watch", None)))
    };

    let expired = list(
        401,
        &[(
            "WWW-Authenticate",
            r#"Bearer error="invalid_token",error_description="The access token expired""#,
        )],
        r#"{"error": "invalid_token"}"#,
    );
    assert_eq!(expired.code, PluginErrorCode::AuthFailed);
    assert!(!expired.public_message.contains(TOKEN));

    // MyAnimeList's 403 is "DoS detected etc." and also its answer to a
    // request without client credentials: a refusal for now, not a 429.
    let refused = list(403, &[], r#"{"message":"","error":"forbidden"}"#);
    assert_eq!(refused.code, PluginErrorCode::UpstreamUnavailable);
    assert!(!refused.public_message.contains("not found"));
    let refused_without_body = list(403, &[], "");
    assert_eq!(
        refused_without_body.code,
        PluginErrorCode::UpstreamUnavailable
    );

    let missing = list(404, &[], r#"{"error": "not_found", "message": ""}"#);
    assert_eq!(missing.code, PluginErrorCode::Permanent);
    assert!(missing.public_message.contains("not found"));

    let limited = list(429, &[("Retry-After", "30")], "");
    assert_eq!(
        (limited.code, limited.retry_after_seconds),
        (PluginErrorCode::RateLimited, Some(30))
    );

    assert_eq!(
        list(503, &[], "").code,
        PluginErrorCode::UpstreamUnavailable
    );
    let bad_request = list(400, &[], r#"{"error": "invalid_parameters"}"#);
    assert_eq!(bad_request.code, PluginErrorCode::Permanent);
    assert!(!bad_request.public_message.contains("not found"));

    assert_eq!(list(200, &[], "not json").code, PluginErrorCode::Permanent);
    let no_entries = list(200, &[], r#"{"paging": {}}"#);
    assert_eq!(no_entries.code, PluginErrorCode::Permanent);
    assert!(
        !no_entries.public_message.contains("not found"),
        "a malformed answer is not an empty or vanished list"
    );
}

#[test]
fn unknown_sources_and_health_are_unsupported() {
    let http = RecordedHttp::new();
    for source in ["status:rewatching", "watching", "user_list", "status:"] {
        assert_eq!(
            err(fetch(&http, request(source, None))).code,
            PluginErrorCode::Unsupported,
            "{source}"
        );
    }
    assert!(http.urls().is_empty());

    match block_on(run(
        &http,
        PluginListCommand::Health(ListPluginHealthRequest {}),
    )) {
        PluginListCommandResult::Health(result) => {
            assert_eq!(err(result).code, PluginErrorCode::Unsupported)
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn account_reads_the_member_identity_and_followable_statuses() {
    let http = RecordedHttp::new().with(
        ACCOUNT_URL,
        200,
        r#"{"id": 990001, "name": "fixture-member",
            "picture": "https://cdn.example.test/userimages/990001.jpg",
            "location": "", "joined_at": "2030-01-01T00:00:00+00:00"}"#,
    );
    let account = ok(account_of(&http, TOKEN));
    assert_eq!(account.external_user_id, "990001");
    assert_eq!(account.username, "fixture-member");
    assert_eq!(account.display_name, None);
    assert_eq!(
        account.avatar_url.as_deref(),
        Some("https://cdn.example.test/userimages/990001.jpg")
    );
    assert!(account.owned_lists.is_empty());

    // Every status the account offers is a source the descriptor declares.
    let declared: Vec<_> = descriptor().list_provider().unwrap().groups[0]
        .items
        .iter()
        .map(|item| {
            (
                item.source_type.clone(),
                item.name.clone(),
                item.kinds.clone(),
            )
        })
        .collect();
    let offered: Vec<_> = account
        .statuses
        .iter()
        .map(|status| {
            (
                status.key.clone(),
                status.label.clone(),
                status.kinds.clone(),
            )
        })
        .collect();
    assert_eq!(offered, declared);

    let sent = &http.requests()[0];
    assert_eq!(
        sent.headers.get("Authorization").map(String::as_str),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(!sent.url.contains(TOKEN));
}

#[test]
fn account_errors_and_partial_identities() {
    let no_picture = RecordedHttp::new().with(
        ACCOUNT_URL,
        200,
        r#"{"id": 990002, "name": "fixture-member-two", "picture": "http://cdn.example.test/p.jpg"}"#,
    );
    assert_eq!(ok(account_of(&no_picture, TOKEN)).avatar_url, None);

    let no_id = RecordedHttp::new().with(ACCOUNT_URL, 200, r#"{"name": "fixture-member"}"#);
    assert_eq!(
        err(account_of(&no_id, TOKEN)).code,
        PluginErrorCode::Permanent
    );

    let revoked = RecordedHttp::new().with(ACCOUNT_URL, 401, r#"{"error": "invalid_token"}"#);
    assert_eq!(
        err(account_of(&revoked, TOKEN)).code,
        PluginErrorCode::AuthFailed
    );

    let refused = RecordedHttp::new().with(ACCOUNT_URL, 403, "");
    assert_eq!(
        err(account_of(&refused, TOKEN)).code,
        PluginErrorCode::UpstreamUnavailable
    );

    let nothing = RecordedHttp::new();
    assert_eq!(
        err(account_of(&nothing, "")).code,
        PluginErrorCode::AuthFailed
    );
    assert!(nothing.urls().is_empty());
}
