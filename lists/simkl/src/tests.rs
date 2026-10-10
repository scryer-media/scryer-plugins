use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListPluginAccountRequest, ListPluginHealthRequest, PluginDescriptor, PluginErrorCode,
    PluginResult,
};

use super::*;

const CLIENT_ID: &str = "fixture-client-id";
const TOKEN: &str = "simkl_at_fixture0token0value";

const ACTIVITIES: &str = r#"{
  "all": "2035-03-01T10:00:00Z",
  "settings": { "all": "2035-01-01T10:00:00Z" },
  "tv_shows": {
    "all": "2035-02-01T10:00:00Z",
    "rated_at": null,
    "playback": null,
    "plantowatch": "2035-02-01T10:00:00Z",
    "watching": "2035-01-20T10:00:00Z",
    "completed": null,
    "hold": null,
    "dropped": null,
    "removed_from_list": null
  },
  "anime": {
    "all": "2035-02-02T10:00:00Z",
    "rated_at": null,
    "playback": null,
    "plantowatch": null,
    "watching": "2035-02-02T10:00:00Z",
    "completed": null,
    "hold": null,
    "dropped": null,
    "removed_from_list": null
  },
  "movies": {
    "all": "2035-02-03T10:00:00Z",
    "rated_at": null,
    "playback": null,
    "plantowatch": "2035-02-03T10:00:00Z",
    "completed": null,
    "dropped": null,
    "removed_from_list": null
  },
  "custom_lists": { "lists": { "all": null } }
}"#;

// Simkl's ID-only form: each entry is its media block's ids and nothing
// else.
const SHOWS: &str = r#"{
  "shows": [
    {
      "show": {
        "ids": {
          "simkl": 9900101,
          "slug": "fixture-serial-one",
          "imdb": "tt9900101",
          "tvdb": "880101",
          "tmdb": "770101"
        }
      }
    },
    { "show": { "ids": { "tvdb": 880102 } } },
    { "show": { "ids": { "simkl": 9900101, "slug": "fixture-serial-one" } } }
  ]
}"#;

const ANIME: &str = r#"{
  "anime": [
    {
      "show": {
        "ids": {
          "simkl": 9900201,
          "slug": "fixture-anime-season-two",
          "mal": "990201",
          "anilist": 990201,
          "anidb": "990211",
          "kitsu": "990221",
          "tvdb": "880201",
          "tmdb": "770201",
          "imdb": "tt9900201"
        }
      }
    },
    { "show": { "ids": { "simkl": 9900202, "mal": "990202", "tvdb": "880201" } } },
    { "show": { "ids": { "simkl": 9900205, "tmdb": "770205", "mal": "990205" } } },
    { "show": { "ids": { "simkl": 9900207, "mal": 990207 } } }
  ]
}"#;

/// The same anime status narrowed to anime movies.
const ANIME_MOVIES: &str = r#"{
  "anime": [
    { "show": { "ids": { "simkl": 9900205, "tmdb": "770205", "mal": "990205" } } }
  ]
}"#;

const MOVIES: &str = r#"{
  "movies": [
    {
      "movie": {
        "ids": {
          "simkl": 9900301,
          "slug": "fixture-feature-one",
          "imdb": "tt9900301",
          "tmdb": "770301"
        }
      }
    },
    { "movie": { "ids": { "simkl": 9900302, "tmdb": 770302, "tvdb": "880302" } } }
  ]
}"#;

const SETTINGS: &str = r#"{
  "user": {
    "name": "fixture_member",
    "joined_at": "2030-06-12T14:23:08.000Z",
    "gender": "",
    "avatar": "https://simkl.in/avatars/00/0000fixture/user_100.jpg",
    "bio": "",
    "loc": null,
    "age": ""
  },
  "account": {
    "id": 990001,
    "timezone": "UTC",
    "type": "free",
    "anime_title_language": "en"
  }
}"#;

fn api(path: &str) -> String {
    format!(
        "https://api.simkl.com{path}?client_id={CLIENT_ID}&app-name=scryer&app-version={}",
        env!("CARGO_PKG_VERSION")
    )
}

fn activities_url() -> String {
    api("/sync/activities")
}

fn items_url(library: &str, status: &str) -> String {
    format!(
        "{}&extended=ids_only",
        api(&format!("/sync/all-items/{library}/{status}"))
    )
}

fn anime_movies_url(status: &str) -> String {
    format!("{}&anime_type=movies", items_url("anime", status))
}

/// Every URL one status read sends after the activity read, in order.
fn library_urls(libraries: &[&str], status: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for library in libraries {
        urls.push(items_url(library, status));
        if *library == "anime" {
            urls.push(anime_movies_url(status));
        }
    }
    urls
}

fn credential() -> ListCredential {
    ListCredential {
        access_token: TOKEN.to_string(),
        token_type: Some("bearer".to_string()),
        external_user_id: Some("990001".to_string()),
        username: Some("fixture_member".to_string()),
    }
}

fn request(status: &str, kind: Option<&str>, since: Option<&str>) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: status.to_string(),
        params: kind
            .map(|kind| BTreeMap::from([(PARAM_TYPE.to_string(), kind.to_string())]))
            .unwrap_or_default(),
        credential: Some(credential()),
        page_cursor: None,
        since_fingerprint: since.map(str::to_string),
    }
}

fn library_http(status: &str, activities: &str) -> RecordedHttp {
    RecordedHttp::new()
        .with(&activities_url(), 200, activities)
        .with(&items_url("shows", status), 200, SHOWS)
        .with(&items_url("anime", status), 200, ANIME)
        .with(&anime_movies_url(status), 200, ANIME_MOVIES)
        .with(&items_url("movies", status), 200, MOVIES)
}

fn fetch_ok(http: &RecordedHttp, request: &ListPluginFetchRequest) -> ListPluginFetchResponse {
    block_on(fetch(http, CLIENT_ID, request)).unwrap()
}

fn fetch_err(http: &RecordedHttp, request: &ListPluginFetchRequest) -> PluginError {
    block_on(fetch(http, CLIENT_ID, request)).unwrap_err()
}

fn ids_of(item: &ListPluginItem) -> Vec<(&str, &str, Option<&str>)> {
    item.external_ids
        .iter()
        .map(|id| (id.source.as_str(), id.id.as_str(), id.kind.as_deref()))
        .collect()
}

fn find<'a>(items: &'a [ListPluginItem], key: &str) -> &'a ListPluginItem {
    items
        .iter()
        .find(|item| item.item_key == key)
        .unwrap_or_else(|| panic!("no item {key}"))
}

fn list_descriptor() -> scryer_plugin_sdk::ListProviderDescriptor {
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
    assert_eq!(decoded.id, "simkl-list");
    assert_eq!(list.provider_type, "simkl");
    assert_eq!(
        list.auth,
        ListProviderAuth::MemberAccount {
            flow: ListAccountFlow::AuthorizationCode { pkce: true },
            exchange: ListAccountExchange::SmgRelay,
            byo_app: false,
            scopes: vec!["media:read".to_string()],
        }
    );
    assert!(list.capabilities.account);
    assert!(!list.capabilities.health);
    assert!(list.capabilities.requires_member_credential);
    assert_eq!(list.allowed_hosts, vec!["api.simkl.com".to_string()]);
    assert_eq!(list.rate_limit_seconds, Some(2));
    assert!(
        list.config_fields.is_empty(),
        "no credential may live in plugin config"
    );
    assert!(list.url_patterns.is_empty(), "nothing here is public");
    assert_eq!(
        list.coverage,
        vec![
            ListMediaKind::Movie,
            ListMediaKind::Series,
            ListMediaKind::Anime
        ]
    );
    let notes: Vec<_> = list
        .notes
        .iter()
        .map(|note| note.text_key.as_str())
        .collect();
    assert_eq!(notes, vec!["lists.note.simkl_anime_seasons"]);
}

#[test]
fn every_status_is_a_personal_six_hour_source() {
    let list = list_descriptor();
    assert_eq!(list.groups.len(), 1);
    assert_eq!(list.groups[0].auth_badge, ListAuthBadge::MemberAccount);
    let items: Vec<_> = list.groups[0]
        .items
        .iter()
        .filter(|item| item.source_type != SOURCE_LIST)
        .collect();
    let sources: Vec<_> = items.iter().map(|item| item.source_type.as_str()).collect();
    assert_eq!(
        sources,
        vec!["watching", "plantowatch", "hold", "completed", "dropped"]
    );
    let all = vec![
        ListMediaKind::Movie,
        ListMediaKind::Series,
        ListMediaKind::Anime,
    ];
    let shows_only = vec![ListMediaKind::Series, ListMediaKind::Anime];
    for item in items {
        assert!(item.personal, "{}", item.id);
        assert_eq!(item.default_interval_seconds, 6 * 60 * 60, "{}", item.id);
        let status = Status::parse(&item.source_type).unwrap();
        let movies = !matches!(status, Status::Watching | Status::OnHold);
        assert_eq!(
            item.kinds,
            if movies {
                all.clone()
            } else {
                shows_only.clone()
            }
        );
        assert_eq!(item.params.len(), 1);
        let param = &item.params[0];
        assert_eq!(param.key, "type");
        assert_eq!(param.param_type, ListSourceParamType::Enum);
        assert!(!param.required);
        let expected: &[&str] = if movies {
            &["all", "movies", "shows", "anime"]
        } else {
            &["all", "shows", "anime"]
        };
        assert_eq!(param.options, expected, "{}", item.id);
    }
}

#[test]
fn each_status_and_type_reads_only_its_libraries() {
    for status in Status::ALL {
        let key = status.key();
        let movies = !matches!(status, Status::Watching | Status::OnHold);
        let mut cases: Vec<(Option<&str>, Vec<&str>)> = vec![
            (
                None,
                if movies {
                    vec!["shows", "anime", "movies"]
                } else {
                    vec!["shows", "anime"]
                },
            ),
            (
                Some("all"),
                if movies {
                    vec!["shows", "anime", "movies"]
                } else {
                    vec!["shows", "anime"]
                },
            ),
            (Some("shows"), vec!["shows"]),
            (Some("anime"), vec!["anime"]),
            (Some(" Anime "), vec!["anime"]),
        ];
        if movies {
            cases.push((Some("movies"), vec!["movies"]));
        }
        for (kind, libraries) in cases {
            let http = library_http(key, ACTIVITIES);
            let response = fetch_ok(&http, &request(key, kind, None));
            let mut expected = vec![activities_url()];
            expected.extend(library_urls(&libraries, key));
            assert_eq!(http.urls(), expected, "{key} {kind:?}");
            assert!(!response.unchanged);
            assert!(response.next_cursor.is_none());
            assert_eq!(response.list_name.as_deref(), Some(status.label()));
            assert_eq!(response.total_hint, Some(response.items.len() as u32));

            let hints: BTreeSet<_> = response
                .items
                .iter()
                .map(|item| format!("{:?}", item.kind_hint.unwrap()))
                .collect();
            let mut want = BTreeSet::new();
            for library in &libraries {
                match *library {
                    "shows" => {
                        want.insert("Series".to_string());
                    }
                    "anime" => {
                        want.insert("Anime".to_string());
                        want.insert("Movie".to_string());
                    }
                    _ => {
                        want.insert("Movie".to_string());
                    }
                }
            }
            assert_eq!(hints, want, "{key} {kind:?}");
            let ranks: Vec<_> = response.items.iter().map(|item| item.rank).collect();
            let expected_ranks: Vec<_> = (1..=response.items.len() as u32).map(Some).collect();
            assert_eq!(ranks, expected_ranks);
        }
    }
}

#[test]
fn movies_are_refused_for_statuses_simkl_does_not_have() {
    for status in [SOURCE_WATCHING, SOURCE_ON_HOLD] {
        let http = library_http(status, ACTIVITIES);
        let error = fetch_err(&http, &request(status, Some("movies"), None));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig, "{status}");
        assert!(http.urls().is_empty());
    }
    let http = library_http(SOURCE_COMPLETED, ACTIVITIES);
    let error = fetch_err(&http, &request(SOURCE_COMPLETED, Some("episodes"), None));
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(http.urls().is_empty());
}

#[test]
fn shows_carry_their_ids_and_a_simkl_key() {
    let http = library_http(SOURCE_WATCHING, ACTIVITIES);
    let response = fetch_ok(&http, &request(SOURCE_WATCHING, Some("shows"), None));
    assert_eq!(response.items.len(), 2, "the repeated entry is dropped");

    let show = find(&response.items, "simkl:show:9900101");
    assert_eq!(show.rank, Some(1));
    assert_eq!(show.kind_hint, Some(ListMediaKind::Series));
    assert_eq!(show.title, None, "the ID-only form carries no title");
    assert_eq!(show.year, None);
    assert_eq!(show.season, None);
    assert_eq!(show.format, None);
    assert_eq!(
        ids_of(show),
        vec![
            ("tmdb", "770101", Some("series")),
            ("imdb", "tt9900101", Some("series")),
            ("tvdb", "880101", Some("series")),
            ("simkl", "9900101", Some("series")),
        ]
    );

    // Simkl always sends its own id; without one the strongest other id keys
    // the entry.
    let fallback = find(&response.items, "tvdb:series:880102");
    assert_eq!(fallback.rank, Some(2));
    assert_eq!(ids_of(fallback), vec![("tvdb", "880102", Some("series"))]);
}

#[test]
fn anime_seasons_stay_apart_and_anime_movies_are_movies() {
    let http = library_http(SOURCE_WATCHING, ACTIVITIES);
    let response = fetch_ok(&http, &request(SOURCE_WATCHING, Some("anime"), None));
    assert_eq!(
        http.urls(),
        vec![
            activities_url(),
            items_url("anime", SOURCE_WATCHING),
            anime_movies_url(SOURCE_WATCHING),
        ]
    );
    let keys: Vec<_> = response
        .items
        .iter()
        .map(|item| item.item_key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![
            "simkl:anime:9900201",
            "simkl:anime:9900202",
            "simkl:anime:9900205",
            "simkl:anime:9900207",
        ],
        "every season is its own item"
    );

    let season_two = find(&response.items, "simkl:anime:9900201");
    assert_eq!(season_two.kind_hint, Some(ListMediaKind::Anime));
    assert_eq!(season_two.season, None, "the ID-only form maps no season");
    assert_eq!(season_two.format, None);
    assert_eq!(
        ids_of(season_two),
        vec![
            ("tmdb", "770201", Some("series")),
            ("imdb", "tt9900201", Some("series")),
            ("tvdb", "880201", Some("series")),
            ("simkl", "9900201", Some("anime")),
            ("mal", "990201", Some("anime")),
            ("anilist", "990201", Some("anime")),
            ("anidb", "990211", Some("anime")),
            ("kitsu", "990221", Some("anime")),
        ]
    );

    let feature = find(&response.items, "simkl:anime:9900205");
    assert_eq!(feature.kind_hint, Some(ListMediaKind::Movie));
    assert_eq!(feature.season, None, "a movie has no season");
    assert_eq!(
        ids_of(feature),
        vec![
            ("tmdb", "770205", Some("movie")),
            ("simkl", "9900205", Some("anime")),
            ("mal", "990205", Some("anime")),
        ]
    );

    let unmapped = find(&response.items, "simkl:anime:9900207");
    assert_eq!(unmapped.kind_hint, Some(ListMediaKind::Anime));
    assert_eq!(
        ids_of(unmapped),
        vec![
            ("simkl", "9900207", Some("anime")),
            ("mal", "990207", Some("anime")),
        ]
    );
}

#[test]
fn a_failed_anime_movie_read_fails_the_whole_status() {
    let http = RecordedHttp::new()
        .with(&activities_url(), 200, ACTIVITIES)
        .with(&items_url("anime", SOURCE_WATCHING), 200, ANIME)
        .with(&anime_movies_url(SOURCE_WATCHING), 503, "");
    let error = fetch_err(&http, &request(SOURCE_WATCHING, Some("anime"), None));
    assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
}

#[test]
fn every_library_read_is_id_only_and_never_a_full_payload() {
    for status in Status::ALL {
        let http = library_http(status.key(), ACTIVITIES);
        fetch_ok(&http, &request(status.key(), None, None));
        for url in http.urls().iter().skip(1) {
            assert!(url.contains("/sync/all-items/"), "{url}");
            assert!(url.contains("&extended=ids_only"), "{url}");
            assert!(!url.contains("full"), "{url}");
            assert!(!url.contains("date_from"), "{url}");
        }
    }
}

#[test]
fn movies_map_to_movie_items() {
    let http = library_http(SOURCE_PLAN_TO_WATCH, ACTIVITIES);
    let response = fetch_ok(&http, &request(SOURCE_PLAN_TO_WATCH, Some("movies"), None));
    assert_eq!(response.items.len(), 2);

    let first = &response.items[0];
    assert_eq!(first.item_key, "simkl:movie:9900301");
    assert_eq!(first.kind_hint, Some(ListMediaKind::Movie));
    assert_eq!(
        ids_of(first),
        vec![
            ("tmdb", "770301", Some("movie")),
            ("imdb", "tt9900301", Some("movie")),
            ("simkl", "9900301", Some("movie")),
        ]
    );
    let second = &response.items[1];
    assert_eq!(
        ids_of(second),
        vec![
            ("tmdb", "770302", Some("movie")),
            ("tvdb", "880302", Some("movie")),
            ("simkl", "9900302", Some("movie")),
        ]
    );
}

#[test]
fn unchanged_activity_skips_the_library_read() {
    let http = library_http(SOURCE_WATCHING, ACTIVITIES);
    let first = fetch_ok(&http, &request(SOURCE_WATCHING, None, None));
    let fingerprint = first.fingerprint.clone().unwrap();
    assert!(fingerprint.starts_with("simkl:v2:"));
    assert!(!fingerprint.contains(TOKEN));

    let again = library_http(SOURCE_WATCHING, ACTIVITIES);
    let second = fetch_ok(&again, &request(SOURCE_WATCHING, None, Some(&fingerprint)));
    assert!(second.unchanged);
    assert!(second.items.is_empty());
    assert_eq!(second.fingerprint.as_deref(), Some(fingerprint.as_str()));
    assert_eq!(again.urls(), vec![activities_url()]);

    let moved = ACTIVITIES.replace("2035-02-02T10:00:00Z", "2035-02-09T10:00:00Z");
    let changed = library_http(SOURCE_WATCHING, &moved);
    let third = fetch_ok(
        &changed,
        &request(SOURCE_WATCHING, None, Some(&fingerprint)),
    );
    assert!(!third.unchanged);
    assert_eq!(third.items, first.items);
    assert_ne!(third.fingerprint.as_deref(), Some(fingerprint.as_str()));
    assert_eq!(changed.urls().len(), 4);
}

#[test]
fn the_fingerprint_follows_only_the_libraries_read() {
    let shows_only = fetch_ok(
        &library_http(SOURCE_WATCHING, ACTIVITIES),
        &request(SOURCE_WATCHING, Some("shows"), None),
    )
    .fingerprint
    .unwrap();

    // Anime activity does not touch a shows-only source.
    let anime_moved = ACTIVITIES.replace("2035-02-02T10:00:00Z", "2035-02-09T10:00:00Z");
    let http = library_http(SOURCE_WATCHING, &anime_moved);
    let response = fetch_ok(
        &http,
        &request(SOURCE_WATCHING, Some("shows"), Some(&shows_only)),
    );
    assert!(response.unchanged);

    // Another status moving in the same library does not touch it either.
    let other_status = ACTIVITIES.replace(
        r#""plantowatch": "2035-02-01T10:00:00Z""#,
        r#""plantowatch": "2035-02-09T10:00:00Z""#,
    );
    let http = library_http(SOURCE_WATCHING, &other_status);
    let response = fetch_ok(
        &http,
        &request(SOURCE_WATCHING, Some("shows"), Some(&shows_only)),
    );
    assert!(response.unchanged);
    assert_eq!(http.urls(), vec![activities_url()]);

    // Its own status moving, or an item leaving the library, rereads it.
    for moved in [
        ACTIVITIES.replace(
            r#""watching": "2035-01-20T10:00:00Z""#,
            r#""watching": "2035-02-09T10:00:00Z""#,
        ),
        ACTIVITIES.replacen(
            r#""removed_from_list": null"#,
            r#""removed_from_list": "2035-02-09T10:00:00Z""#,
            1,
        ),
    ] {
        let http = library_http(SOURCE_WATCHING, &moved);
        let response = fetch_ok(
            &http,
            &request(SOURCE_WATCHING, Some("shows"), Some(&shows_only)),
        );
        assert!(!response.unchanged);
        assert_eq!(response.items.len(), 2);
        assert_eq!(http.urls().len(), 2);
    }

    // The same timestamps under another status are another fingerprint.
    let completed = fetch_ok(
        &library_http(SOURCE_COMPLETED, ACTIVITIES),
        &request(SOURCE_COMPLETED, Some("shows"), None),
    )
    .fingerprint
    .unwrap();
    assert_ne!(completed, shows_only);
}

#[test]
fn a_new_member_without_activity_is_still_fingerprinted() {
    // Simkl's shape for a member who has not touched the library yet.
    let fresh = r#"{
      "all": null,
      "settings": { "all": null },
      "tv_shows": {
        "all": null, "rated_at": null, "playback": null, "plantowatch": null,
        "watching": null, "completed": null, "hold": null, "dropped": null,
        "removed_from_list": null
      },
      "anime": {
        "all": null, "rated_at": null, "playback": null, "plantowatch": null,
        "watching": null, "completed": null, "hold": null, "dropped": null,
        "removed_from_list": null
      },
      "movies": {
        "all": null, "rated_at": null, "playback": null, "plantowatch": null,
        "completed": null, "dropped": null, "removed_from_list": null
      },
      "custom_lists": { "lists": { "all": null } }
    }"#;
    let empty = RecordedHttp::new()
        .with(&activities_url(), 200, fresh)
        .with(&items_url("shows", SOURCE_DROPPED), 200, "{}")
        .with(&items_url("anime", SOURCE_DROPPED), 200, "{}")
        .with(&anime_movies_url(SOURCE_DROPPED), 200, "{}")
        .with(&items_url("movies", SOURCE_DROPPED), 200, "{}");
    let first = fetch_ok(&empty, &request(SOURCE_DROPPED, None, None));
    assert!(first.items.is_empty());
    let fingerprint = first.fingerprint.unwrap();
    assert!(fingerprint.starts_with("simkl:v2:"));

    let again = RecordedHttp::new().with(&activities_url(), 200, fresh);
    let second = fetch_ok(&again, &request(SOURCE_DROPPED, None, Some(&fingerprint)));
    assert!(second.unchanged);
    assert_eq!(again.urls(), vec![activities_url()]);
}

#[test]
fn unusable_activity_falls_back_to_a_content_fingerprint() {
    for activities in [
        r#"{"tv_shows": {"watching": "2035-01-20T10:00:00Z"}}"#,
        r#"{"tv_shows": {"all": "2035-01-20T10:00:00Z", "removed_from_list": null}}"#,
        r#"{"tv_shows": {"watching": 7, "removed_from_list": null}}"#,
        r#"{}"#,
        "",
        "null",
    ] {
        let http = RecordedHttp::new()
            .with(&activities_url(), 200, activities)
            .with(&items_url("shows", SOURCE_WATCHING), 200, SHOWS);
        let first = fetch_ok(&http, &request(SOURCE_WATCHING, Some("shows"), None));
        assert_eq!(first.items.len(), 2, "{activities}");
        let fingerprint = first.fingerprint.unwrap();
        assert!(!fingerprint.starts_with("simkl:"), "{activities}");

        let again = RecordedHttp::new()
            .with(&activities_url(), 200, activities)
            .with(&items_url("shows", SOURCE_WATCHING), 200, SHOWS);
        let second = fetch_ok(
            &again,
            &request(SOURCE_WATCHING, Some("shows"), Some(&fingerprint)),
        );
        assert!(second.unchanged, "{activities}");
        assert_eq!(again.urls().len(), 2, "the library is always read");
    }
}

#[test]
fn empty_libraries_are_empty_lists() {
    for body in [
        "{}",
        "",
        "  ",
        "null",
        "[]",
        r#"{"shows": null}"#,
        r#"{"movies": []}"#,
    ] {
        let http = RecordedHttp::new()
            .with(&activities_url(), 200, ACTIVITIES)
            .with(&items_url("shows", SOURCE_COMPLETED), 200, body);
        let response = fetch_ok(&http, &request(SOURCE_COMPLETED, Some("shows"), None));
        assert!(response.items.is_empty(), "{body:?}");
        assert!(!response.unchanged);
    }
}

#[test]
fn credential_less_fetches_are_refused_before_any_request() {
    let http = library_http(SOURCE_WATCHING, ACTIVITIES);
    let missing = ListPluginFetchRequest {
        credential: None,
        ..request(SOURCE_WATCHING, None, None)
    };
    let error = fetch_err(&http, &missing);
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
    assert!(error.public_message.contains("linked Simkl account"));

    let blank = ListPluginFetchRequest {
        credential: Some(ListCredential {
            access_token: "  ".to_string(),
            ..credential()
        }),
        ..request(SOURCE_WATCHING, None, None)
    };
    assert_eq!(fetch_err(&http, &blank).code, PluginErrorCode::AuthFailed);
    assert!(http.urls().is_empty());
}

#[test]
fn an_unregistered_app_refuses_every_call_before_any_request() {
    let http = library_http(SOURCE_WATCHING, ACTIVITIES);
    for client_id in ["", "   "] {
        match block_on(run(
            &http,
            client_id,
            PluginListCommand::Fetch(request(SOURCE_WATCHING, None, None)),
        )) {
            PluginListCommandResult::Fetch(PluginResult::Err(error)) => {
                assert_eq!(error.code, PluginErrorCode::InvalidConfig);
                assert!(error.public_message.contains("no Simkl app id"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match block_on(run(
            &http,
            client_id,
            PluginListCommand::Account(ListPluginAccountRequest {
                credential: credential(),
            }),
        )) {
            PluginListCommandResult::Account(PluginResult::Err(error)) => {
                assert_eq!(error.code, PluginErrorCode::InvalidConfig)
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(http.urls().is_empty());
}

#[test]
fn requests_name_the_app_and_carry_the_token_only_in_a_header() {
    let http = library_http(SOURCE_PLAN_TO_WATCH, ACTIVITIES);
    fetch_ok(&http, &request(SOURCE_PLAN_TO_WATCH, None, None));
    let requests = http.requests();
    assert_eq!(requests.len(), 5);
    for request in requests {
        assert_eq!(request.method.as_deref(), Some("GET"));
        assert!(request.body.is_empty());
        assert!(!request.url.contains(TOKEN), "{}", request.url);
        assert!(request.url.starts_with("https://api.simkl.com/"));
        assert!(request.url.contains("client_id=fixture-client-id"));
        assert!(request.url.contains("app-name=scryer"));
        assert_eq!(
            request.headers.get("Authorization").map(String::as_str),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(
            request.headers.get("User-Agent").map(String::as_str),
            Some(concat!("simkl-list-provider/", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(
            request.headers.get("Accept").map(String::as_str),
            Some("application/json")
        );
        // The client id goes once, in the URL; Content-Type is for writes.
        assert!(!request.headers.contains_key("simkl-api-key"));
        assert!(!request.headers.contains_key("Content-Type"));
    }
}

#[test]
fn upstream_failures_map_to_host_classes() {
    let cases = [
        (401, PluginErrorCode::AuthFailed, false),
        // A 403 Simkl gives no reason for is a refusal retrying cannot fix.
        (403, PluginErrorCode::Permanent, false),
        (404, PluginErrorCode::Permanent, true),
        (412, PluginErrorCode::UpstreamUnavailable, false),
        (429, PluginErrorCode::RateLimited, false),
        (500, PluginErrorCode::UpstreamUnavailable, false),
        (502, PluginErrorCode::UpstreamUnavailable, false),
        (400, PluginErrorCode::Permanent, false),
    ];
    for (status, code, not_found) in cases {
        // A failure on the activity read stops the sync before the library.
        let http = RecordedHttp::new()
            .with_headers(&activities_url(), status, &[("Retry-After", "120")], "")
            .with(&items_url("shows", SOURCE_WATCHING), 200, SHOWS);
        let error = fetch_err(&http, &request(SOURCE_WATCHING, Some("shows"), None));
        assert_eq!(error.code, code, "{status}");
        assert_eq!(
            error.public_message.contains("not found"),
            not_found,
            "{status}"
        );
        if status == 429 {
            assert_eq!(
                error.retry_after_seconds,
                Some(120),
                "an unnamed 429 keeps Retry-After"
            );
        }
        assert_eq!(http.urls(), vec![activities_url()], "{status}");

        // So does a failure on the library read itself.
        let http = RecordedHttp::new()
            .with(&activities_url(), 200, ACTIVITIES)
            .with_headers(
                &items_url("anime", SOURCE_WATCHING),
                status,
                &[("Retry-After", "120")],
                r#"{"error":"fixture_error","code":0,"message":"Fixture failure"}"#,
            );
        let error = fetch_err(&http, &request(SOURCE_WATCHING, Some("anime"), None));
        assert_eq!(error.code, code, "{status}");
    }
}

#[test]
fn rejected_tokens_name_simkls_reason_but_never_the_token() {
    let named = RecordedHttp::new().with(
        &activities_url(),
        401,
        r#"{"error":"invalid_token","code":401,"message":"Token expired"}"#,
    );
    let error = fetch_err(&named, &request(SOURCE_WATCHING, None, None));
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
    assert!(error.public_message.contains("HTTP 401, invalid_token"));

    let echoed = RecordedHttp::new().with(
        &activities_url(),
        401,
        &format!(r#"{{"error":"{TOKEN}","message":"bad token {TOKEN}"}}"#),
    );
    let error = fetch_err(&echoed, &request(SOURCE_WATCHING, None, None));
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
    assert!(!error.public_message.contains(TOKEN));
    assert!(!format!("{error:?}").contains(TOKEN));

    let refused = RecordedHttp::new().with(
        &activities_url(),
        412,
        r#"{"error":"client_id_failed","code":412}"#,
    );
    let error = fetch_err(&refused, &request(SOURCE_WATCHING, None, None));
    assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
    assert!(error.public_message.contains("client_id_failed"));
    assert!(!error.public_message.contains(CLIENT_ID));
}

#[test]
fn a_403_that_only_a_new_link_fixes_asks_for_one() {
    for name in ["insufficient_scope", "oauth2_token_required"] {
        let http = RecordedHttp::new().with(
            &activities_url(),
            403,
            &format!(r#"{{"error":"{name}","code":403,"message":"Fixture refusal"}}"#),
        );
        let error = fetch_err(&http, &request(SOURCE_WATCHING, None, None));
        assert_eq!(error.code, PluginErrorCode::AuthFailed, "{name}");
        assert!(error.public_message.contains(name), "{name}");
    }
    for name in ["forbidden", "private_list"] {
        let http = RecordedHttp::new().with(
            &activities_url(),
            403,
            &format!(r#"{{"error":"{name}","code":403}}"#),
        );
        let error = fetch_err(&http, &request(SOURCE_WATCHING, None, None));
        assert_eq!(error.code, PluginErrorCode::Permanent, "{name}");
        assert!(!error.public_message.contains("not found"), "{name}");
    }
}

#[test]
fn malformed_library_responses_are_permanent_failures() {
    for body in [
        "<html>maintenance</html>",
        r#"{"shows": {"title": "Fixture"}}"#,
        r#"[{"show": {}}]"#,
        r#""text""#,
        r#"{"error": "fixture_error"}"#,
    ] {
        let http = RecordedHttp::new()
            .with(&activities_url(), 200, ACTIVITIES)
            .with(&items_url("shows", SOURCE_WATCHING), 200, body);
        let error = fetch_err(&http, &request(SOURCE_WATCHING, Some("shows"), None));
        assert_eq!(error.code, PluginErrorCode::Permanent, "{body}");
        assert!(!error.public_message.contains("not found"), "{body}");
    }
}

#[test]
fn account_reads_the_identity_and_offers_every_status() {
    let http = settings_http(SETTINGS);
    let account = match block_on(run(
        &http,
        CLIENT_ID,
        PluginListCommand::Account(ListPluginAccountRequest {
            credential: credential(),
        }),
    )) {
        PluginListCommandResult::Account(PluginResult::Ok(account)) => account,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        http.urls(),
        vec![api("/users/settings"), custom_index_url("990001", 1)]
    );
    assert_eq!(http.requests()[0].method.as_deref(), Some("GET"));
    assert_eq!(
        http.requests()[0]
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(account.external_user_id, "990001");
    assert_eq!(account.username, "fixture_member");
    assert_eq!(account.display_name.as_deref(), Some("fixture_member"));
    assert_eq!(
        account.avatar_url.as_deref(),
        Some("https://simkl.in/avatars/00/0000fixture/user_100.jpg")
    );
    assert!(account.owned_lists.is_empty());
    let statuses: Vec<_> = account
        .statuses
        .iter()
        .map(|status| {
            (
                status.key.as_str(),
                status.label.as_str(),
                status.kinds.len(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("watching", "Watching", 2),
            ("plantowatch", "Plan to watch", 3),
            ("hold", "On hold", 2),
            ("completed", "Completed", 3),
            ("dropped", "Dropped", 3),
        ]
    );
    // Every status key is a source the descriptor declares, with the same
    // kinds.
    let list = list_descriptor();
    for status in &account.statuses {
        let item = list.groups[0]
            .items
            .iter()
            .find(|item| item.source_type == status.key)
            .unwrap();
        assert_eq!(item.kinds, status.kinds);
    }
}

#[test]
fn account_tolerates_a_private_profile_but_needs_an_id() {
    let private =
        r#"{"user": {"name": "", "avatar": "/img/default.png"}, "account": {"id": "990002"}}"#;
    let http = settings_http(private);
    let profile = block_on(account(&http, CLIENT_ID, &credential())).unwrap();
    assert_eq!(profile.external_user_id, "990002");
    assert_eq!(profile.username, "990002");
    assert_eq!(profile.display_name, None);
    assert_eq!(profile.avatar_url, None);

    let anonymous = RecordedHttp::new().with(&api("/users/settings"), 200, r#"{"user": {}}"#);
    let error = block_on(account(&anonymous, CLIENT_ID, &credential())).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::Permanent);

    let revoked = RecordedHttp::new().with(
        &api("/users/settings"),
        401,
        r#"{"error":"user_token_failed"}"#,
    );
    let error = block_on(account(&revoked, CLIENT_ID, &credential())).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::AuthFailed);
}

#[test]
fn unknown_sources_and_health_are_unsupported() {
    let http = RecordedHttp::new();
    let other = request("friends", None, None);
    assert_eq!(fetch_err(&http, &other).code, PluginErrorCode::Unsupported);
    match block_on(run(
        &http,
        CLIENT_ID,
        PluginListCommand::Health(ListPluginHealthRequest {}),
    )) {
        PluginListCommandResult::Health(PluginResult::Err(error)) => {
            assert_eq!(error.code, PluginErrorCode::Unsupported)
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(http.urls().is_empty());
}

#[test]
fn the_configured_client_id_wins_over_the_built_in_one() {
    assert_eq!(
        client_id(Some(" fixture-config-id "), "fixture-built-in-id"),
        "fixture-config-id"
    );
    assert_eq!(
        client_id(Some(""), "fixture-built-in-id"),
        "fixture-built-in-id"
    );
    assert_eq!(
        client_id(None, "fixture-built-in-id"),
        "fixture-built-in-id"
    );
    assert_eq!(client_id(None, SIMKL_CLIENT_ID), "");

    let http = RecordedHttp::new()
        .with(
            &api("/users/settings").replace(CLIENT_ID, "fixture-config-id"),
            200,
            SETTINGS,
        )
        .with(
            &custom_index_url("990001", 1).replace(CLIENT_ID, "fixture-config-id"),
            200,
            PREMIUM,
        );
    block_on(account(
        &http,
        client_id(Some("fixture-config-id"), "fixture-built-in-id"),
        &credential(),
    ))
    .unwrap();
    assert!(http.urls()[0].contains("client_id=fixture-config-id"));

    let untouched = RecordedHttp::new();
    let error = block_on(account(
        &untouched,
        client_id(None, SIMKL_CLIENT_ID),
        &credential(),
    ))
    .unwrap_err();
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(untouched.urls().is_empty());
}

fn limited(body: &str, retry_after: &str) -> PluginError {
    let http = RecordedHttp::new()
        .with(&activities_url(), 200, ACTIVITIES)
        .with_headers(
            &items_url("shows", SOURCE_WATCHING),
            429,
            &[("Retry-After", retry_after)],
            body,
        );
    fetch_err(&http, &request(SOURCE_WATCHING, Some("shows"), None))
}

#[test]
fn a_burst_429_retries_shortly_and_ignores_retry_after() {
    let error = limited(r#"{"error":"rate_limit","code":429}"#, "3600");
    assert_eq!(error.code, PluginErrorCode::RateLimited);
    assert_eq!(error.retry_after_seconds, Some(BURST_RETRY_SECONDS));
    assert!(error.public_message.contains("per-second"));
}

#[test]
fn a_spent_daily_allowance_waits_for_the_reset_and_names_whose_it_is() {
    let member = limited(r#"{"error":"user_limit_exceeded","code":429}"#, "5400");
    assert_eq!(member.code, PluginErrorCode::RateLimited);
    assert_eq!(member.retry_after_seconds, Some(5400));
    assert!(member.public_message.contains("this member's daily"));
    assert!(!member.public_message.contains("unavailable"));
    assert!(!member.public_message.contains(TOKEN));

    let app = limited(r#"{"error":"app_limit_exceeded","code":429}"#, "600");
    assert_eq!(app.code, PluginErrorCode::RateLimited);
    assert_eq!(app.retry_after_seconds, Some(600));
    assert!(app.public_message.contains("app's daily"));
}

#[test]
fn a_status_too_large_to_build_is_a_permanent_failure_that_says_why() {
    let http = RecordedHttp::new()
        .with(&activities_url(), 200, ACTIVITIES)
        .with(
            &items_url("shows", SOURCE_COMPLETED),
            400,
            r#"{"error":"max_items","code":400,"message":"Too many episodes: fixture"}"#,
        );
    let error = fetch_err(&http, &request(SOURCE_COMPLETED, Some("shows"), None));
    assert_eq!(error.code, PluginErrorCode::Permanent);
    assert!(error.public_message.contains("max_items"));
    assert!(error.public_message.contains("too large"));
    assert!(!error.public_message.contains("not found"));
    assert!(!error.public_message.contains("Too many episodes"));
}

const PREMIUM: &str = r#"{"error":"premium_only","item":{"title":"Upgrade"}}"#;

fn custom_index_url(user: &str, page: u32) -> String {
    format!(
        "{}&limit=500&page={page}",
        api(&format!("/lists/user/{user}"))
    )
}

fn custom_url(page: u32) -> String {
    format!("{}&limit=500&page={page}", api("/lists/9909"))
}

fn settings_http(settings: &str) -> RecordedHttp {
    let body: Value = serde_json::from_str(settings).unwrap();
    let id = json_id(body.get("account").unwrap().get("id")).unwrap();
    RecordedHttp::new()
        .with(&api("/users/settings"), 200, settings)
        .with(&custom_index_url(&id, 1), 200, PREMIUM)
}

fn custom_request() -> ListPluginFetchRequest {
    let mut request = request(SOURCE_LIST, None, None);
    request.params.insert(PARAM_LIST_ID.into(), "9909".into());
    request
}

fn custom_body(media: &str, item: Value) -> Value {
    serde_json::json!({
        "id":9909, "name":"Fixture custom list", "media_type":media,
        "type":"regular", "updated_at":"2035-01-01T00:00:00Z",
        "pagination":{"page":1,"limit":500,"total_items":1,"total_pages":1},
        "items":[item]
    })
}

fn custom_media(kind: &str, id: u32) -> Value {
    serde_json::json!({"type":kind,"title":"Fixture title","year":2030,
        "ids":{"simkl_id":id,"tmdb":"770301","imdb":"tt9900301"}})
}

#[test]
fn custom_source_uses_the_existing_host_owned_list_contract() {
    let descriptor = list_descriptor();
    let source = descriptor.groups[0]
        .items
        .iter()
        .find(|item| item.source_type == "list")
        .unwrap();
    assert!(source.personal);
    assert_eq!(source.params[0].key, "list_id");
    assert!(source.params[0].required);
    assert_eq!(source.kinds.len(), 3);
}

#[test]
fn custom_discovery_pages_owned_lists_without_fetching_their_contents() {
    let first = serde_json::json!({"pagination":{"page":1,"limit":2,"total_items":3,"total_pages":2},
        "lists":[{"id":991,"name":"Fixture private movies","privacy":"private","media_type":"movies"},
                 {"id":992,"name":"Fixture unlisted TV","privacy":"unlisted","media_type":"tv"}]});
    let second = serde_json::json!({"pagination":{"page":2,"limit":2,"total_items":3,"total_pages":2},
        "lists":[{"id":993,"name":"Fixture anime","media_type":"anime"}]});
    let http = RecordedHttp::new()
        .with(&api("/users/settings"), 200, SETTINGS)
        .with(&custom_index_url("990001", 1), 200, &first.to_string())
        .with(&custom_index_url("990001", 2), 200, &second.to_string());
    let response = block_on(account(&http, CLIENT_ID, &credential())).unwrap();
    assert_eq!(
        response
            .owned_lists
            .iter()
            .map(|list| list.id.as_str())
            .collect::<Vec<_>>(),
        ["991", "992", "993"]
    );
    assert_eq!(response.owned_lists[0].kinds, [ListMediaKind::Movie]);
    assert_eq!(response.owned_lists[1].kinds, [ListMediaKind::Series]);
    assert_eq!(
        response.owned_lists[2].kinds,
        [ListMediaKind::Anime, ListMediaKind::Movie]
    );
    assert_eq!(response.statuses.len(), 5);
    assert_eq!(
        http.urls(),
        [
            api("/users/settings"),
            custom_index_url("990001", 1),
            custom_index_url("990001", 2)
        ]
    );
}

#[test]
fn custom_items_keep_stable_keys_and_typed_ids_for_movies_tv_and_anime() {
    for (media, kind, subtype, expected, key_scope, id_kind) in [
        (
            "movies",
            "movie",
            None,
            ListMediaKind::Movie,
            "movie",
            "movie",
        ),
        ("tv", "tv", None, ListMediaKind::Series, "show", "series"),
        (
            "anime",
            "anime",
            Some("tv"),
            ListMediaKind::Anime,
            "anime",
            "series",
        ),
        (
            "anime",
            "anime",
            Some("movie"),
            ListMediaKind::Movie,
            "anime",
            "movie",
        ),
    ] {
        let mut item = custom_media(kind, 9900301);
        if let Some(subtype) = subtype {
            item["anime_type"] = subtype.into();
        }
        let body = custom_body(media, item);
        let http = RecordedHttp::new().with(&custom_url(1), 200, &body.to_string());
        let response = fetch_ok(&http, &custom_request());
        assert_eq!(response.items.len(), 1);
        let item = &response.items[0];
        assert_eq!(item.item_key, format!("simkl:{key_scope}:9900301"));
        assert_eq!(item.kind_hint, Some(expected));
        assert_eq!(item.rank, Some(1));
        assert!(ids_of(item).contains(&("tmdb", "770301", Some(id_kind))));
        assert!(response.fingerprint.is_none());
        assert_eq!(http.urls(), [custom_url(1)]);
        assert_eq!(
            http.requests()[0].headers["Authorization"],
            format!("Bearer {TOKEN}")
        );
        assert!(!http.urls()[0].contains(TOKEN));
    }
}

#[test]
fn custom_paging_rejects_changed_snapshots_and_reuses_the_owners_order() {
    let mut first = custom_body("movies", custom_media("movie", 9900301));
    first["pagination"] = serde_json::json!({"page":1,"limit":1,"total_items":2,"total_pages":2});
    let mut second = first.clone();
    second["pagination"]["page"] = 2.into();
    second["items"][0] = custom_media("movie", 9900302);
    let http = RecordedHttp::new()
        .with(&custom_url(1), 200, &first.to_string())
        .with(&custom_url(2), 200, &second.to_string());
    let first_response = fetch_ok(&http, &custom_request());
    let mut request = custom_request();
    request.page_cursor = first_response.next_cursor;
    let response = fetch_ok(&http, &request);
    assert_eq!(response.items[0].rank, Some(2));
    assert!(response.next_cursor.is_none());
    assert_eq!(http.urls(), [custom_url(1), custom_url(2)]);
    second["updated_at"] = "2035-01-02T00:00:00Z".into();
    let changed = RecordedHttp::new().with(&custom_url(2), 200, &second.to_string());
    assert_eq!(
        fetch_err(&changed, &request).code,
        PluginErrorCode::Permanent
    );
}

#[test]
fn custom_lists_never_treat_errors_or_partial_data_as_empty_membership() {
    let valid = custom_body("movies", custom_media("movie", 9900301));
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("items");
    let mut truncated = valid.clone();
    truncated["items"] = serde_json::json!([]);
    let mut oversized = valid.clone();
    oversized["pagination"]["total_items"] = 10001.into();
    let mut mismatched = valid.clone();
    mismatched["id"] = 9910.into();
    let mut wrong_kind = valid.clone();
    wrong_kind["items"][0]["type"] = "episode".into();
    let mut unknown_anime = custom_body("anime", custom_media("anime", 9900301));
    unknown_anime["items"][0]["anime_type"] = Value::Null;
    for body in [
        serde_json::from_str(PREMIUM).unwrap(),
        missing,
        truncated,
        oversized,
        mismatched,
        wrong_kind,
        unknown_anime,
    ] {
        let http = RecordedHttp::new().with(&custom_url(1), 200, &body.to_string());
        assert_eq!(
            fetch_err(&http, &custom_request()).code,
            PluginErrorCode::Permanent
        );
    }
    for (status, code) in [
        (401, PluginErrorCode::AuthFailed),
        (403, PluginErrorCode::Permanent),
        (404, PluginErrorCode::Permanent),
        (429, PluginErrorCode::RateLimited),
        (503, PluginErrorCode::UpstreamUnavailable),
    ] {
        let http = RecordedHttp::new().with(&custom_url(1), status, "{}");
        assert_eq!(fetch_err(&http, &custom_request()).code, code);
    }
}

#[test]
fn custom_empty_lists_are_valid_and_auto_lists_are_never_timestamp_short_circuited() {
    let mut body = custom_body("movies", custom_media("movie", 9900301));
    body["type"] = "auto".into();
    let http = RecordedHttp::new().with(&custom_url(1), 200, &body.to_string());
    let mut request = custom_request();
    request.since_fingerprint = Some("2035-01-01T00:00:00Z".into());
    assert!(!fetch_ok(&http, &request).unchanged);
    body["items"] = serde_json::json!([]);
    body["pagination"] = serde_json::json!({"page":1,"limit":500,"total_items":0,"total_pages":0});
    let empty = RecordedHttp::new().with(&custom_url(1), 200, &body.to_string());
    assert!(fetch_ok(&empty, &request).items.is_empty());
}

#[test]
fn custom_bad_ids_cursors_and_missing_credentials_do_not_make_requests() {
    let http = RecordedHttp::new();
    let mut bad = custom_request();
    bad.params
        .insert(PARAM_LIST_ID.into(), "../users/settings".into());
    assert_eq!(fetch_err(&http, &bad).code, PluginErrorCode::InvalidConfig);
    let mut bad = custom_request();
    bad.page_cursor = Some("not-a-cursor".into());
    assert_eq!(fetch_err(&http, &bad).code, PluginErrorCode::InvalidConfig);
    let mut bad = custom_request();
    bad.credential = None;
    assert_eq!(fetch_err(&http, &bad).code, PluginErrorCode::AuthFailed);
    assert!(http.urls().is_empty());
}

#[test]
fn custom_discovery_fails_on_malformed_pages_and_outages_instead_of_hiding_lists() {
    for (status, body) in [(200, "{}"), (503, "{}"), (200, r#"{"error":"unexpected"}"#)] {
        let http = RecordedHttp::new()
            .with(&api("/users/settings"), 200, SETTINGS)
            .with(&custom_index_url("990001", 1), status, body);
        assert!(block_on(account(&http, CLIENT_ID, &credential())).is_err());
    }
}

fn auto_pages() -> (Value, Value) {
    let mut first = custom_body("movies", custom_media("movie", 1));
    first["type"] = "auto".into();
    first["items"] = (1..=500)
        .map(|id| custom_media("movie", id))
        .collect::<Vec<_>>()
        .into();
    first["pagination"] =
        serde_json::json!({"page":1,"limit":500,"total_items":501,"total_pages":2});
    let mut second = first.clone();
    second["pagination"]["page"] = 2.into();
    second["items"] = serde_json::json!([custom_media("movie", 501)]);
    (first, second)
}

#[test]
fn custom_auto_pages_verify_membership_before_returning_any_items() {
    let (first, second) = auto_pages();
    let http = RecordedHttp::new()
        .with(&custom_url(1), 200, &first.to_string())
        .with(&custom_url(2), 200, &second.to_string());
    let result = fetch_ok(&http, &custom_request());
    assert_eq!(result.items.len(), 501);
    assert_eq!(result.items.last().unwrap().rank, Some(501));
    assert!(result.next_cursor.is_none());
    assert_eq!(
        http.urls(),
        [custom_url(1), custom_url(2), custom_url(1), custom_url(2)]
    );
}

struct RebuiltAutoList {
    reads: std::cell::Cell<usize>,
    before: RecordedHttp,
    after: RecordedHttp,
}

impl ListHttp for RebuiltAutoList {
    async fn send(&self, request: PluginHttpRequest) -> Result<PluginHttpResponse, PluginError> {
        let read = self.reads.get();
        self.reads.set(read + 1);
        if read == 0 {
            self.before.send(request).await
        } else {
            self.after.send(request).await
        }
    }
}

#[test]
fn custom_auto_rebuild_with_unchanged_metadata_is_rejected() {
    for reorder_only in [false, true] {
        let (first, second) = auto_pages();
        let mut rebuilt = first.clone();
        if reorder_only {
            rebuilt["items"].as_array_mut().unwrap().swap(0, 1);
        } else {
            // Same size and timestamp, and no duplicate at the page boundary.
            rebuilt["items"][0] = custom_media("movie", 502);
        }
        let http = RebuiltAutoList {
            reads: std::cell::Cell::new(0),
            before: RecordedHttp::new().with(&custom_url(1), 200, &first.to_string()),
            after: RecordedHttp::new()
                .with(&custom_url(1), 200, &rebuilt.to_string())
                .with(&custom_url(2), 200, &second.to_string()),
        };
        let error = block_on(fetch(&http, CLIENT_ID, &custom_request())).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("changed during pagination"));
        assert_eq!(http.reads.get(), 3);
    }
}

#[test]
fn custom_auto_duplicate_pages_and_verification_outages_fail_closed() {
    let (first, mut second) = auto_pages();
    second["items"][0] = custom_media("movie", 1);
    let http = RecordedHttp::new()
        .with(&custom_url(1), 200, &first.to_string())
        .with(&custom_url(2), 200, &second.to_string());
    assert_eq!(
        fetch_err(&http, &custom_request()).code,
        PluginErrorCode::Permanent
    );
    let (_, second) = auto_pages();
    let http = RebuiltAutoList {
        reads: std::cell::Cell::new(0),
        before: RecordedHttp::new().with(&custom_url(1), 200, &first.to_string()),
        after: RecordedHttp::new().with(&custom_url(1), 503, "{}").with(
            &custom_url(2),
            200,
            &second.to_string(),
        ),
    };
    assert_eq!(
        block_on(fetch(&http, CLIENT_ID, &custom_request()))
            .unwrap_err()
            .code,
        PluginErrorCode::UpstreamUnavailable
    );
}

#[test]
fn custom_private_list_is_gone_but_scope_errors_still_require_reconnection() {
    for (name, code, gone) in [
        ("private_list", PluginErrorCode::Permanent, true),
        ("insufficient_scope", PluginErrorCode::AuthFailed, false),
        ("oauth2_token_required", PluginErrorCode::AuthFailed, false),
        ("other", PluginErrorCode::Permanent, false),
    ] {
        let http = RecordedHttp::new().with(
            &custom_url(1),
            403,
            &serde_json::json!({"error":name}).to_string(),
        );
        let error = fetch_err(&http, &custom_request());
        assert_eq!(error.code, code);
        assert_eq!(error.public_message.contains("not found"), gone);
    }
}
