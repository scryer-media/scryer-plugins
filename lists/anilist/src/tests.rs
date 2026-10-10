use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{ListPluginAccountRequest, ListPluginHealthRequest, PluginDescriptor};
use serde_json::{Value, json};

use super::*;

const TOKEN: &str = "fixture-header.fixture-claims.fixture-signature";

/// A status list in the shape AniList documents for `MediaListCollection`,
/// covering every anime format and the gaps AniList leaves in its data.
const WATCHING: &str = r#"{
  "data": {
    "MediaListCollection": {
      "hasNextChunk": false,
      "user": {
        "name": "fixture-member",
        "mediaListOptions": { "animeList": { "customLists": ["Fixture Favourites"] } }
      },
      "lists": [
        {
          "name": "Watching",
          "isCustomList": false,
          "entries": [
            {
              "mediaId": 900001,
              "status": "CURRENT",
              "media": {
                "id": 900001, "idMal": 800001, "format": "TV", "status": "RELEASING",
                "seasonYear": 2031, "startDate": { "year": 2031 },
                "title": { "userPreferred": "Fixture Orbit Academy", "romaji": "Fikusucha Obito Gakuen", "english": "Fixture Orbit Academy" },
                "genres": ["Action", "Sci-Fi"], "averageScore": 78
              }
            },
            {
              "mediaId": 900002,
              "status": "CURRENT",
              "media": {
                "id": 900002, "idMal": null, "format": "TV_SHORT", "status": "NOT_YET_RELEASED",
                "seasonYear": 2032, "startDate": { "year": null },
                "title": { "userPreferred": null, "romaji": "Fikusucha Mini Gekijou", "english": null },
                "genres": [], "averageScore": null
              }
            },
            {
              "mediaId": 900003,
              "status": "CURRENT",
              "media": {
                "id": 900003, "idMal": 800003, "format": "MOVIE", "status": "FINISHED",
                "seasonYear": null, "startDate": { "year": 2029 },
                "title": { "userPreferred": null, "romaji": null, "english": "Fixture Lantern Film" },
                "genres": ["Drama"], "averageScore": 0
              }
            },
            {
              "mediaId": 900004,
              "status": "CURRENT",
              "media": {
                "id": 900004, "idMal": 800004, "format": "ONA", "status": "CANCELLED",
                "seasonYear": 2030, "startDate": { "year": 2030 },
                "title": { "userPreferred": "Fixture Stream Serial" }, "genres": [], "averageScore": 55
              }
            },
            {
              "mediaId": 900005,
              "status": "CURRENT",
              "media": {
                "id": 900005, "idMal": 800005, "format": "OVA", "status": "HIATUS",
                "startDate": { "year": 2028 }, "title": { "userPreferred": "Fixture Shelf Episode" }
              }
            },
            {
              "mediaId": 900006,
              "status": "CURRENT",
              "media": {
                "id": 900006, "format": "SPECIAL", "status": "FINISHED",
                "title": { "userPreferred": "Fixture Recap Special" }
              }
            },
            {
              "mediaId": 900007,
              "status": "CURRENT",
              "media": {
                "id": 900007, "format": "MUSIC", "status": "FINISHED",
                "title": { "userPreferred": "Fixture Opening Clip" }
              }
            },
            {
              "mediaId": 900008,
              "status": "CURRENT",
              "media": { "id": 900008, "format": null, "status": null, "title": { "userPreferred": "Fixture Unsorted" } }
            },
            { "mediaId": 900009, "status": "CURRENT", "media": null },
            {
              "mediaId": 900001,
              "status": "CURRENT",
              "media": { "id": 900001, "idMal": 800001, "format": "TV", "title": { "userPreferred": "Fixture Orbit Academy" } }
            }
          ]
        }
      ]
    }
  }
}"#;

const VIEWER: &str = r#"{
  "data": {
    "Viewer": {
      "id": 4242,
      "name": "fixture-member",
      "avatar": {
        "large": "https://img.example.test/avatar/large/fixture.png",
        "medium": "https://img.example.test/avatar/medium/fixture.png"
      },
      "mediaListOptions": { "animeList": { "customLists": ["Fixture Favourites", "Fixture Queue"] } }
    }
  }
}"#;

fn credential() -> ListCredential {
    ListCredential {
        access_token: TOKEN.to_string(),
        token_type: Some("Bearer".to_string()),
        external_user_id: Some("4242".to_string()),
        username: Some("fixture-member".to_string()),
    }
}

fn request(source_type: &str, key: &str, value: &str) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: source_type.to_string(),
        params: BTreeMap::from([(key.to_string(), value.to_string())]),
        credential: Some(credential()),
        page_cursor: None,
        since_fingerprint: None,
    }
}

fn status_request(status: &str) -> ListPluginFetchRequest {
    request(SOURCE_STATUS, PARAM_STATUS, status)
}

fn custom_request(name: &str) -> ListPluginFetchRequest {
    request(SOURCE_CUSTOM_LIST, PARAM_LIST, name)
}

fn media(id: u64, format: &str, title: &str) -> Value {
    json!({
        "id": id,
        "idMal": null,
        "format": format,
        "status": "FINISHED",
        "seasonYear": 2031,
        "startDate": { "year": 2031 },
        "title": { "userPreferred": title, "romaji": title, "english": null },
        "genres": ["Drama"],
        "averageScore": 70
    })
}

fn entry(status: &str, id: u64) -> Value {
    json!({
        "mediaId": id,
        "status": status,
        "media": media(id, "TV", &format!("Fixture Serial {id}")),
    })
}

fn group(name: &str, custom: bool, entries: Vec<Value>) -> Value {
    json!({ "name": name, "isCustomList": custom, "entries": entries })
}

fn collection(groups: Vec<Value>, has_next: bool, custom_lists: &[&str]) -> String {
    json!({
        "data": {
            "MediaListCollection": {
                "hasNextChunk": has_next,
                "user": {
                    "name": "fixture-member",
                    "mediaListOptions": { "animeList": { "customLists": custom_lists } }
                },
                "lists": groups
            }
        }
    })
    .to_string()
}

fn answering(body: &str) -> RecordedHttp {
    RecordedHttp::new().with(API_URL, 200, body)
}

/// The one request a fetch sent, with its decoded GraphQL body.
fn sent(http: &RecordedHttp) -> (PluginHttpRequest, Value) {
    let mut requests = http.requests();
    assert_eq!(requests.len(), 1, "every fetch sends exactly one request");
    let request = requests.remove(0);
    let body = serde_json::from_slice(&request.body).unwrap();
    (request, body)
}

fn keys(response: &ListPluginFetchResponse) -> Vec<(&str, Option<u32>)> {
    response
        .items
        .iter()
        .map(|item| (item.item_key.as_str(), item.rank))
        .collect()
}

fn assert_no_secret(error: &PluginError) {
    assert!(!error.public_message.contains(TOKEN), "{error:?}");
    assert!(
        error
            .debug_message
            .as_deref()
            .is_none_or(|message| !message.contains(TOKEN)),
        "{error:?}"
    );
}

fn list_descriptor() -> ListProviderDescriptor {
    descriptor().list_provider().cloned().unwrap()
}

#[test]
fn descriptor_round_trips_and_passes_host_checks() {
    let original = descriptor();
    let bytes = serde_json::to_vec(&original).unwrap();
    let decoded: PluginDescriptor = serde_json::from_slice(&bytes).unwrap();
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

    let list = list_descriptor();
    assert_eq!(list.provider_type, "anilist");
    assert_eq!(
        list.auth,
        ListProviderAuth::MemberAccount {
            flow: ListAccountFlow::AuthorizationCode { pkce: false },
            exchange: ListAccountExchange::SmgRelay,
            byo_app: true,
            scopes: Vec::new(),
        }
    );
    assert!(list.capabilities.account);
    assert!(!list.capabilities.health);
    assert!(list.capabilities.requires_member_credential);
    assert_eq!(list.allowed_hosts, vec!["graphql.anilist.co".to_string()]);
    assert_eq!(list.rate_limit_seconds, Some(2));
    assert!(
        list.config_fields.is_empty(),
        "no credential may live in plugin config"
    );
    assert!(list.url_patterns.is_empty());
    assert_eq!(
        list.coverage,
        vec![ListMediaKind::Anime, ListMediaKind::Movie]
    );

    let items: Vec<&ListProviderItem> = list.groups.iter().flat_map(|g| &g.items).collect();
    let sources: Vec<&str> = items.iter().map(|item| item.source_type.as_str()).collect();
    assert_eq!(sources, vec![SOURCE_STATUS, SOURCE_CUSTOM_LIST]);
    for item in &items {
        assert!(item.personal, "{} must be personal", item.id);
        assert_eq!(item.default_interval_seconds, 12 * 60 * 60);
        assert!(item.params.iter().all(|param| param.required));
    }
    let options: Vec<&str> = items[0].params[0]
        .options
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        options,
        vec![
            "current",
            "planning",
            "completed",
            "repeating",
            "paused",
            "dropped"
        ]
    );
}

#[test]
fn each_status_list_reads_that_status_for_the_linked_member() {
    for status in &STATUSES {
        let other = if status.anilist == "PLANNING" {
            "CURRENT"
        } else {
            "PLANNING"
        };
        let body = collection(
            vec![
                group(status.label, false, vec![entry(status.anilist, 900001)]),
                group(
                    "Fixture Favourites",
                    true,
                    vec![
                        // Hidden from the status lists, kept in a custom list.
                        entry(status.anilist, 900002),
                        entry(status.anilist, 900001),
                        entry(other, 900003),
                    ],
                ),
            ],
            false,
            &["Fixture Favourites"],
        );
        let http = answering(&body);
        let response = block_on(fetch(&http, &status_request(status.key))).unwrap();

        let (sent, body) = sent(&http);
        assert_eq!(sent.url, API_URL);
        assert_eq!(sent.method.as_deref(), Some("POST"));
        assert_eq!(
            sent.headers.get("Authorization"),
            Some(&format!("Bearer {TOKEN}"))
        );
        assert_eq!(
            sent.headers.get("Content-Type").map(String::as_str),
            Some("application/json")
        );
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("MediaListCollection(") && query.contains("type: ANIME"));
        assert_eq!(
            body["variables"],
            json!({ "userId": 4242, "status": status.anilist, "chunk": 1, "perChunk": 500 })
        );

        assert_eq!(response.list_name.as_deref(), Some(status.label));
        assert_eq!(
            response.list_url,
            Some(format!(
                "https://anilist.co/user/fixture-member/animelist/{}",
                status.label
            ))
        );
        assert_eq!(
            keys(&response),
            vec![("anilist:900001", Some(1)), ("anilist:900002", Some(2))],
            "{}",
            status.key
        );
        assert!(response.next_cursor.is_none());
        assert!(response.fingerprint.is_some());
    }
}

#[test]
fn entries_map_ids_formats_kinds_and_release_state() {
    let http = answering(WATCHING);
    let response = block_on(fetch(&http, &status_request("current"))).unwrap();

    type Expected = (
        &'static str,
        ListMediaKind,
        Option<&'static str>,
        Option<bool>,
        Option<&'static str>,
        Option<i32>,
    );
    let expected: Vec<Expected> = vec![
        (
            "anilist:900001",
            ListMediaKind::Anime,
            Some("TV"),
            Some(true),
            Some("Fixture Orbit Academy"),
            Some(2031),
        ),
        (
            "anilist:900002",
            ListMediaKind::Anime,
            Some("TV_SHORT"),
            Some(false),
            Some("Fikusucha Mini Gekijou"),
            Some(2032),
        ),
        (
            "anilist:900003",
            ListMediaKind::Movie,
            Some("MOVIE"),
            Some(true),
            Some("Fixture Lantern Film"),
            Some(2029),
        ),
        (
            "anilist:900004",
            ListMediaKind::Anime,
            Some("ONA"),
            None,
            Some("Fixture Stream Serial"),
            Some(2030),
        ),
        (
            "anilist:900005",
            ListMediaKind::Anime,
            Some("OVA"),
            Some(true),
            Some("Fixture Shelf Episode"),
            Some(2028),
        ),
        (
            "anilist:900006",
            ListMediaKind::Anime,
            Some("SPECIAL"),
            Some(true),
            Some("Fixture Recap Special"),
            None,
        ),
        (
            "anilist:900007",
            ListMediaKind::Anime,
            Some("MUSIC"),
            Some(true),
            Some("Fixture Opening Clip"),
            None,
        ),
        (
            "anilist:900008",
            ListMediaKind::Anime,
            None,
            None,
            Some("Fixture Unsorted"),
            None,
        ),
        (
            "anilist:900009",
            ListMediaKind::Anime,
            None,
            None,
            None,
            None,
        ),
    ];
    assert_eq!(
        response.items.len(),
        expected.len(),
        "the duplicate is dropped"
    );
    for (rank, (item, (key, kind, format, released, title, year))) in
        response.items.iter().zip(expected).enumerate()
    {
        assert_eq!(item.item_key, key);
        assert_eq!(item.rank, Some(rank as u32 + 1), "{key}");
        assert_eq!(item.kind_hint, Some(kind), "{key}");
        assert_eq!(item.format.as_deref(), format, "{key}");
        assert_eq!(item.released, released, "{key}");
        assert_eq!(item.title.as_deref(), title, "{key}");
        assert_eq!(item.year, year, "{key}");
        assert_eq!(item.season, None);
        assert_eq!(item.external_ids[0].source, "anilist");
        assert_eq!(item.external_ids[0].id, key.trim_start_matches("anilist:"));
        assert!(
            item.external_ids.iter().all(|id| id.kind.is_none()),
            "AniList and MyAnimeList ids name films and series alike"
        );
    }

    let orbit = &response.items[0];
    let ids: Vec<(&str, &str)> = orbit
        .external_ids
        .iter()
        .map(|id| (id.source.as_str(), id.id.as_str()))
        .collect();
    assert_eq!(ids, vec![("anilist", "900001"), ("mal", "800001")]);
    assert_eq!(
        orbit.genres,
        vec!["Action".to_string(), "Sci-Fi".to_string()]
    );
    let rating = orbit.provider_rating.as_ref().unwrap();
    assert_eq!((rating.scale.as_str(), rating.value), ("anilist", 78.0));

    let short = &response.items[1];
    assert_eq!(short.external_ids.len(), 1, "no MyAnimeList id is invented");
    assert!(short.provider_rating.is_none());
    assert!(
        response.items[2].provider_rating.is_none(),
        "an unscored title has no rating"
    );

    assert_eq!(kind_for_format(Some("movie")), ListMediaKind::Movie);
    assert_eq!(kind_for_format(Some("TV")), ListMediaKind::Anime);
    assert_eq!(kind_for_format(None), ListMediaKind::Anime);
}

#[test]
fn a_custom_list_reads_only_the_named_list() {
    let body = collection(
        vec![
            group(
                "Watching",
                false,
                vec![entry("CURRENT", 900001), entry("CURRENT", 900002)],
            ),
            group(
                "Fixture Favourites",
                true,
                vec![entry("COMPLETED", 900002), entry("DROPPED", 900004)],
            ),
            group("Fixture Queue", true, vec![entry("PLANNING", 900005)]),
        ],
        false,
        &["Fixture Favourites", "Fixture Queue"],
    );
    let http = answering(&body);
    let response = block_on(fetch(&http, &custom_request("  fixture favourites "))).unwrap();

    let (_, sent) = sent(&http);
    assert_eq!(
        sent["variables"],
        json!({ "userId": 4242, "chunk": 1, "perChunk": 500 }),
        "a custom list reads every status"
    );
    assert_eq!(response.list_name.as_deref(), Some("Fixture Favourites"));
    assert_eq!(
        response.list_url.as_deref(),
        Some("https://anilist.co/user/fixture-member/animelist/Fixture%20Favourites")
    );
    assert_eq!(
        keys(&response),
        vec![("anilist:900002", Some(1)), ("anilist:900004", Some(2))]
    );
}

#[test]
fn an_exact_custom_list_name_wins_over_a_case_insensitive_one() {
    let body = collection(
        vec![
            group("Fixture Queue", true, vec![entry("PLANNING", 900005)]),
            group("fixture queue", true, vec![entry("PLANNING", 900006)]),
        ],
        false,
        &["Fixture Queue", "fixture queue"],
    );
    let response = block_on(fetch(&answering(&body), &custom_request("fixture queue"))).unwrap();
    assert_eq!(response.list_name.as_deref(), Some("fixture queue"));
    assert_eq!(keys(&response), vec![("anilist:900006", Some(1))]);
}

#[test]
fn a_missing_custom_list_is_not_found_but_an_empty_one_is_empty() {
    let body = collection(
        vec![group("Watching", false, vec![entry("CURRENT", 900001)])],
        false,
        &["Fixture Queue"],
    );
    let error = block_on(fetch(&answering(&body), &custom_request("Fixture Archive"))).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::Permanent);
    assert!(error.public_message.contains("not found"));
    assert!(
        !error.public_message.contains("Fixture Archive"),
        "errors never name the member's lists"
    );

    let empty = block_on(fetch(&answering(&body), &custom_request("Fixture Queue"))).unwrap();
    assert!(empty.items.is_empty());
    assert_eq!(empty.list_name.as_deref(), Some("Fixture Queue"));
    assert!(empty.next_cursor.is_none());
}

#[test]
fn large_lists_page_through_chunks_without_a_fingerprint() {
    let first_chunk: Vec<Value> = (1..=u64::from(PER_CHUNK))
        .map(|n| entry("PLANNING", 910_000 + n))
        .collect();
    let http = answering(&collection(
        vec![group("Planning", false, first_chunk)],
        true,
        &[],
    ));
    let first = block_on(fetch(&http, &status_request("planning"))).unwrap();
    assert_eq!(first.next_cursor.as_deref(), Some("2"));
    assert!(first.fingerprint.is_none());
    assert!(!first.unchanged);
    assert_eq!(first.items.len(), 500);
    assert_eq!(first.items[0].rank, Some(1));
    assert_eq!(first.items[499].rank, Some(500));
    assert_eq!(sent(&http).1["variables"]["chunk"], 1);

    let http = answering(&collection(
        vec![group("Planning", false, vec![entry("PLANNING", 920_001)])],
        false,
        &[],
    ));
    let next = ListPluginFetchRequest {
        page_cursor: first.next_cursor.clone(),
        since_fingerprint: Some("v1:0000000000000000:1".to_string()),
        ..status_request("planning")
    };
    let second = block_on(fetch(&http, &next)).unwrap();
    assert_eq!(sent(&http).1["variables"]["chunk"], 2);
    assert_eq!(keys(&second), vec![("anilist:920001", Some(501))]);
    assert!(second.next_cursor.is_none());
    assert!(second.fingerprint.is_none());
    assert!(!second.unchanged);
}

#[test]
fn an_unchanged_single_chunk_short_circuits_on_the_stored_fingerprint() {
    let http = answering(WATCHING);
    let first = block_on(fetch(&http, &status_request("current"))).unwrap();
    let again = ListPluginFetchRequest {
        since_fingerprint: first.fingerprint.clone(),
        ..status_request("current")
    };
    let second = block_on(fetch(&http, &again)).unwrap();
    assert!(second.unchanged);
    assert!(second.items.is_empty());
    assert_eq!(second.fingerprint, first.fingerprint);
}

#[test]
fn a_collection_at_anilists_entry_cap_fails_instead_of_shrinking() {
    assert_eq!(MAX_CHUNKS, 22);
    let full_chunk: Vec<Value> = (1..=u64::from(PER_CHUNK))
        .map(|n| entry("COMPLETED", 930_000 + n))
        .collect();
    let last = ListPluginFetchRequest {
        page_cursor: Some(MAX_CHUNKS.to_string()),
        ..status_request("completed")
    };
    for has_next in [false, true] {
        let http = answering(&collection(
            vec![group("Completed", false, full_chunk.clone())],
            has_next,
            &[],
        ));
        let error = block_on(fetch(&http, &last)).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("11,000"));
        assert!(!error.public_message.contains("not found"));
    }

    let before_cap = ListPluginFetchRequest {
        page_cursor: Some((MAX_CHUNKS - 1).to_string()),
        ..status_request("completed")
    };
    let http = answering(&collection(
        vec![group("Completed", false, full_chunk)],
        true,
        &[],
    ));
    let response = block_on(fetch(&http, &before_cap)).unwrap();
    assert_eq!(response.next_cursor.as_deref(), Some("22"));
    assert_eq!(response.items[0].rank, Some(10_001));

    for cursor in ["0", "23", "two"] {
        let http = RecordedHttp::new();
        let request = ListPluginFetchRequest {
            page_cursor: Some(cursor.to_string()),
            ..status_request("completed")
        };
        let error = block_on(fetch(&http, &request)).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent, "{cursor}");
        assert!(http.requests().is_empty(), "{cursor}");
    }
}

#[test]
fn account_reports_the_viewer_statuses_and_custom_lists() {
    let http = answering(VIEWER);
    let result = block_on(run(
        &http,
        PluginListCommand::Account(ListPluginAccountRequest {
            credential: credential(),
        }),
    ));
    let account = match result {
        PluginListCommandResult::Account(PluginResult::Ok(account)) => account,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(account.external_user_id, "4242");
    assert_eq!(account.username, "fixture-member");
    assert_eq!(account.display_name.as_deref(), Some("fixture-member"));
    assert_eq!(
        account.avatar_url.as_deref(),
        Some("https://img.example.test/avatar/large/fixture.png")
    );
    let statuses: Vec<(&str, &str)> = account
        .statuses
        .iter()
        .map(|status| (status.key.as_str(), status.label.as_str()))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("current", "Watching"),
            ("planning", "Planning"),
            ("completed", "Completed"),
            ("repeating", "Rewatching"),
            ("paused", "Paused"),
            ("dropped", "Dropped"),
        ]
    );
    let lists: Vec<(&str, &str)> = account
        .owned_lists
        .iter()
        .map(|list| (list.id.as_str(), list.name.as_str()))
        .collect();
    assert_eq!(
        lists,
        vec![
            ("Fixture Favourites", "Fixture Favourites"),
            ("Fixture Queue", "Fixture Queue")
        ]
    );
    assert!(
        account
            .statuses
            .iter()
            .all(|status| status.kinds == vec![ListMediaKind::Anime, ListMediaKind::Movie])
    );

    let (sent, body) = sent(&http);
    assert_eq!(sent.method.as_deref(), Some("POST"));
    assert_eq!(
        sent.headers.get("Authorization"),
        Some(&format!("Bearer {TOKEN}"))
    );
    assert!(body["query"].as_str().unwrap().contains("Viewer"));
}

#[test]
fn an_account_answer_without_a_viewer_is_a_rejected_token() {
    let http = answering(r#"{ "data": { "Viewer": null } }"#);
    let error = block_on(account(&http, &credential())).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
}

#[test]
fn fetches_without_a_usable_account_are_refused_before_any_request() {
    let http = RecordedHttp::new();
    let unlinked = ListPluginFetchRequest {
        credential: None,
        ..status_request("current")
    };
    let blank_token = ListPluginFetchRequest {
        credential: Some(ListCredential {
            access_token: "   ".to_string(),
            ..credential()
        }),
        ..status_request("current")
    };
    let broken_token = ListPluginFetchRequest {
        credential: Some(ListCredential {
            access_token: "fixture\r\nX-Injected: 1".to_string(),
            ..credential()
        }),
        ..status_request("current")
    };
    let nameless = ListPluginFetchRequest {
        credential: Some(ListCredential {
            external_user_id: None,
            username: None,
            ..credential()
        }),
        ..status_request("current")
    };
    for request in [unlinked, blank_token, broken_token, nameless] {
        let error = block_on(fetch(&http, &request)).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert_no_secret(&error);
    }
    let blank_account = ListCredential {
        access_token: String::new(),
        ..credential()
    };
    assert_eq!(
        block_on(account(&http, &blank_account)).unwrap_err().code,
        PluginErrorCode::AuthFailed
    );
    assert!(http.requests().is_empty());
}

#[test]
fn a_link_without_a_numeric_id_names_the_member_by_username() {
    let http = answering(&collection(Vec::new(), false, &[]));
    let request = ListPluginFetchRequest {
        credential: Some(ListCredential {
            external_user_id: Some("fixture-opaque-id".to_string()),
            ..credential()
        }),
        ..status_request("current")
    };
    block_on(fetch(&http, &request)).unwrap();
    assert_eq!(
        sent(&http).1["variables"],
        json!({ "userName": "fixture-member", "status": "CURRENT", "chunk": 1, "perChunk": 500 })
    );
}

#[test]
fn the_token_travels_only_in_the_authorization_header() {
    let http = answering(WATCHING);
    block_on(fetch(&http, &status_request("current"))).unwrap();
    let (sent, _) = sent(&http);
    assert!(!sent.url.contains(TOKEN));
    assert!(!String::from_utf8_lossy(&sent.body).contains(TOKEN));
    let carrying: Vec<&String> = sent
        .headers
        .iter()
        .filter(|(_, value)| value.contains(TOKEN))
        .map(|(key, _)| key)
        .collect();
    assert_eq!(carrying, vec!["Authorization"]);
}

#[test]
fn http_failures_map_to_host_classes() {
    let cases = [
        (401, PluginErrorCode::AuthFailed, false),
        (403, PluginErrorCode::UpstreamUnavailable, false),
        (404, PluginErrorCode::Permanent, true),
        (429, PluginErrorCode::RateLimited, false),
        (500, PluginErrorCode::UpstreamUnavailable, false),
        (502, PluginErrorCode::UpstreamUnavailable, false),
        (418, PluginErrorCode::Permanent, false),
    ];
    for (status, code, not_found) in cases {
        let http = RecordedHttp::new().with_headers(API_URL, status, &[("Retry-After", "61")], "");
        let error = block_on(fetch(&http, &status_request("current"))).unwrap_err();
        assert_eq!(error.code, code, "{status}");
        assert_eq!(
            error.public_message.contains("not found"),
            not_found,
            "{status}"
        );
        if status == 429 {
            assert_eq!(error.retry_after_seconds, Some(61));
        }
        assert_no_secret(&error);
    }
    let html = answering("<html><body>maintenance</body></html>");
    assert_eq!(
        block_on(fetch(&html, &status_request("current")))
            .unwrap_err()
            .code,
        PluginErrorCode::Permanent
    );
    let empty = answering(r#"{ "data": { "MediaListCollection": null } }"#);
    assert_eq!(
        block_on(fetch(&empty, &status_request("current")))
            .unwrap_err()
            .code,
        PluginErrorCode::Permanent
    );
}

#[test]
fn graphql_errors_map_to_host_classes_even_with_http_200() {
    let cases = [
        (
            400,
            "Invalid token",
            400,
            PluginErrorCode::AuthFailed,
            false,
        ),
        (
            200,
            "Unauthorized.",
            401,
            PluginErrorCode::AuthFailed,
            false,
        ),
        (
            200,
            "Too Many Requests.",
            429,
            PluginErrorCode::RateLimited,
            false,
        ),
        (404, "Not Found.", 404, PluginErrorCode::Permanent, true),
        (404, "User not found", 404, PluginErrorCode::Permanent, true),
        (404, "Private User", 404, PluginErrorCode::Permanent, true),
        (200, "User not found", 404, PluginErrorCode::Permanent, true),
        (
            403,
            "The AniList API has been temporarily disabled due to severe stability issues.",
            403,
            PluginErrorCode::UpstreamUnavailable,
            false,
        ),
        (
            400,
            "Cannot query field \"fixtureField\" on type \"MediaList\".",
            400,
            PluginErrorCode::Permanent,
            false,
        ),
        (
            200,
            "Internal Server Error",
            500,
            PluginErrorCode::UpstreamUnavailable,
            false,
        ),
    ];
    for (http_status, message, status, code, not_found) in cases {
        let body = json!({
            "data": null,
            "errors": [{ "message": message, "status": status, "locations": [{ "line": 2, "column": 3 }] }]
        })
        .to_string();
        let http =
            RecordedHttp::new().with_headers(API_URL, http_status, &[("Retry-After", "30")], &body);
        let error = block_on(fetch(&http, &status_request("current"))).unwrap_err();
        assert_eq!(error.code, code, "{message}");
        assert_eq!(
            error.public_message.contains("not found"),
            not_found,
            "{message}"
        );
        assert!(
            !error.public_message.contains(message),
            "AniList's own wording is not passed on"
        );
        if code == PluginErrorCode::RateLimited {
            assert_eq!(error.retry_after_seconds, Some(30));
        }
        assert_no_secret(&error);
    }

    // A rejected token outranks anything else AniList reports alongside it.
    let mixed = json!({
        "data": null,
        "errors": [
            { "message": "Not Found.", "status": 404 },
            { "message": "Invalid token", "status": 400 }
        ]
    })
    .to_string();
    let error = block_on(fetch(&answering(&mixed), &status_request("current"))).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::AuthFailed);

    // Partial data with errors is still a failure, never a shortened list.
    let partial = json!({
        "data": { "MediaListCollection": { "hasNextChunk": false, "lists": [] } },
        "errors": [{ "message": "Internal Server Error", "status": 500 }]
    })
    .to_string();
    let error = block_on(fetch(&answering(&partial), &status_request("current"))).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);

    let account_error =
        json!({ "data": null, "errors": [{ "message": "Invalid token", "status": 400 }] })
            .to_string();
    let http = RecordedHttp::new().with(API_URL, 400, &account_error);
    assert_eq!(
        block_on(account(&http, &credential())).unwrap_err().code,
        PluginErrorCode::AuthFailed
    );
}

#[test]
fn unknown_sources_bad_params_and_health_are_rejected() {
    let http = RecordedHttp::new();
    let other = ListPluginFetchRequest {
        source_type: "seasonal".to_string(),
        ..status_request("current")
    };
    assert_eq!(
        block_on(fetch(&http, &other)).unwrap_err().code,
        PluginErrorCode::Unsupported
    );
    assert_eq!(
        block_on(fetch(&http, &status_request("binging")))
            .unwrap_err()
            .code,
        PluginErrorCode::InvalidConfig
    );
    let no_status = ListPluginFetchRequest {
        params: BTreeMap::new(),
        ..status_request("current")
    };
    assert_eq!(
        block_on(fetch(&http, &no_status)).unwrap_err().code,
        PluginErrorCode::InvalidConfig
    );
    assert_eq!(
        block_on(fetch(&http, &custom_request("  ")))
            .unwrap_err()
            .code,
        PluginErrorCode::InvalidConfig
    );
    assert!(http.requests().is_empty());

    match block_on(run(
        &http,
        PluginListCommand::Health(ListPluginHealthRequest {}),
    )) {
        PluginListCommandResult::Health(PluginResult::Err(error)) => {
            assert_eq!(error.code, PluginErrorCode::Unsupported)
        }
        other => panic!("unexpected {other:?}"),
    }

    // AniList's own enum spelling is accepted too.
    let http = answering(&collection(Vec::new(), false, &[]));
    block_on(fetch(&http, &status_request("REPEATING"))).unwrap();
    assert_eq!(sent(&http).1["variables"]["status"], "REPEATING");
}

/// `body` with the member's total anime entry count set, or the
/// `hasNextChunk` flag replaced (`None` drops it).
fn edited(body: &str, total: Option<u64>, has_next: Option<Option<Value>>) -> String {
    let mut body: Value = serde_json::from_str(body).unwrap();
    let collection = body.pointer_mut("/data/MediaListCollection").unwrap();
    if let Some(total) = total {
        collection["user"]["statistics"] = json!({ "anime": { "count": total } });
    }
    match has_next {
        Some(Some(value)) => collection["hasNextChunk"] = value,
        Some(None) => {
            collection.as_object_mut().unwrap().remove("hasNextChunk");
        }
        None => {}
    }
    body.to_string()
}

#[test]
fn the_collection_query_reads_the_members_total_entry_count() {
    let http = answering(WATCHING);
    block_on(fetch(&http, &status_request("current"))).unwrap();
    let query = sent(&http).1["query"].as_str().unwrap().to_string();
    assert!(query.contains("statistics { anime { count } }"), "{query}");
}

#[test]
fn a_short_status_list_of_a_collection_past_the_cap_fails() {
    let short = collection(
        vec![group("Planning", false, vec![entry("PLANNING", 940_001)])],
        false,
        &[],
    );
    for total in [u64::from(COLLECTION_CAP), u64::from(COLLECTION_CAP) + 4_000] {
        let http = answering(&edited(&short, Some(total), None));
        let error = block_on(fetch(&http, &status_request("planning"))).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent, "{total}");
        assert!(error.public_message.contains("11,000"));
        assert!(!error.public_message.contains("not found"));
    }

    // Below the cap, or with no statistics at all, the short list stands.
    for body in [
        edited(&short, Some(u64::from(COLLECTION_CAP) - 1), None),
        short.clone(),
    ] {
        let http = answering(&body);
        let response = block_on(fetch(&http, &status_request("planning"))).unwrap();
        assert_eq!(keys(&response), vec![("anilist:940001", Some(1))]);
        assert!(response.fingerprint.is_some());
    }
}

#[test]
fn a_full_chunk_without_a_next_chunk_flag_fails_instead_of_ending_the_list() {
    let full_chunk: Vec<Value> = (1..=u64::from(PER_CHUNK))
        .map(|n| entry("COMPLETED", 950_000 + n))
        .collect();
    let full = collection(vec![group("Completed", false, full_chunk)], false, &[]);
    for flag in [None, Some(Value::Null), Some(json!("yes"))] {
        let http = answering(&edited(&full, None, Some(flag.clone())));
        let error = block_on(fetch(&http, &status_request("completed"))).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent, "{flag:?}");
        assert!(!error.public_message.contains("not found"));
    }

    // A short chunk without the flag is the end of the list.
    let short = collection(
        vec![group("Completed", false, vec![entry("COMPLETED", 950_999)])],
        false,
        &[],
    );
    let http = answering(&edited(&short, None, Some(None)));
    let response = block_on(fetch(&http, &status_request("completed"))).unwrap();
    assert_eq!(keys(&response), vec![("anilist:950999", Some(1))]);
    assert!(response.next_cursor.is_none());
}
