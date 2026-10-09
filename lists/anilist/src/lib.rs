//! AniList list provider.
//!
//! AniList keeps every anime a member tracks in one list collection, grouped
//! by watching status and by the member's own custom lists. This plugin reads
//! a linked member's collection through AniList's GraphQL API with the
//! member's access token, one status list or one custom list per
//! subscription. AniList's public charts (seasonal, trending, popular) come
//! from the metadata gateway, so every source here is personal.
//!
//! AniList does not infer the member from the token, so every collection
//! query names the member by the AniList user id the account link recorded.
//! A collection is read in chunks of 500 entries, one chunk per fetch, and
//! AniList serves at most the 11,000 most recently updated entries of a
//! collection. A read that reaches that cap fails instead of returning a
//! shortened list, because the host would take every missing title as having
//! left the list. A status list is filtered from the whole collection, so
//! every chunk also reads the member's total anime entry count and fails
//! once that reaches the cap, however short the status list itself is.
//!
//! Entries carry the AniList id and, when AniList knows it, the MyAnimeList
//! id. Both are kindless: the metadata gateway maps them onto TVDB and TMDb.
//! AniList films are hinted as movies and every other format as anime.

use std::collections::BTreeSet;

use list_provider_common::error::{
    auth_failed, invalid_config, missing_param, not_found, permanent, plugin_error, rate_limited,
    retry_after_seconds, unavailable, unsupported_source,
};
use list_provider_common::http::{HostHttp, ListHttp, encode_component, get};
use list_provider_common::ids::{dedupe_and_rank, json_id, json_text, json_year};
use list_provider_common::page::{numeric_cursor, single_page};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{
    ListAccountExchange, ListAccountFlow, ListAccountList, ListAccountStatus, ListAuthBadge,
    ListCredential, ListExternalId, ListMediaKind, ListNoteTone, ListPluginAccountResponse,
    ListPluginFetchRequest, ListPluginFetchResponse, ListPluginItem, ListProviderAuth,
    ListProviderCapabilities, ListProviderDescriptor, ListProviderGroup, ListProviderItem,
    ListProviderNote, ListProviderRating, ListProviderTile, ListSourceParam, ListSourceParamType,
    PluginDescriptor, PluginError, PluginErrorCode, PluginResult, ProviderDescriptor,
};
use serde_json::{Map, Value, json};

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/runtime-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/list-v1.0.0"],
    generate_all,
});

list_provider_common::list_component_main!(descriptor = descriptor, handler = handle_command,);

pub const PLUGIN_ID: &str = "anilist-list";
pub const PROVIDER_TYPE: &str = "anilist";
pub const SOURCE_STATUS: &str = "status";
pub const SOURCE_CUSTOM_LIST: &str = "custom_list";
pub const PARAM_STATUS: &str = "status";
pub const PARAM_LIST: &str = "list";
/// External id sources this plugin emits. Neither carries a kind: one AniList
/// or MyAnimeList id names a film or a series alike.
pub const ID_ANILIST: &str = "anilist";
pub const ID_MAL: &str = "mal";
pub const API_URL: &str = "https://graphql.anilist.co";
const API_HOST: &str = "graphql.anilist.co";
const SITE_BASE: &str = "https://anilist.co";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/json";
/// Entries per collection chunk, the most AniList serves at once.
pub const PER_CHUNK: u32 = 500;
/// AniList returns at most this many entries of one collection.
pub const COLLECTION_CAP: u32 = 11_000;
/// The chunks a capped collection fills, far below the host's hundred pages
/// per sync.
pub const MAX_CHUNKS: u32 = COLLECTION_CAP / PER_CHUNK;
/// Twelve hours, Sonarr's shortest refresh for an AniList list.
const DEFAULT_INTERVAL_SECONDS: u64 = 12 * 60 * 60;
/// AniList allows 30 requests a minute while its rate limit is degraded
/// (90 otherwise), so the host spaces requests two seconds apart.
const RATE_LIMIT_SECONDS: i64 = 2;

/// One AniList list status: the parameter value, AniList's enum value and the
/// label AniList shows on an anime list.
pub struct Status {
    pub key: &'static str,
    pub anilist: &'static str,
    pub label: &'static str,
}

pub static STATUSES: [Status; 6] = [
    Status {
        key: "current",
        anilist: "CURRENT",
        label: "Watching",
    },
    Status {
        key: "planning",
        anilist: "PLANNING",
        label: "Planning",
    },
    Status {
        key: "completed",
        anilist: "COMPLETED",
        label: "Completed",
    },
    Status {
        key: "repeating",
        anilist: "REPEATING",
        label: "Rewatching",
    },
    Status {
        key: "paused",
        anilist: "PAUSED",
        label: "Paused",
    },
    Status {
        key: "dropped",
        anilist: "DROPPED",
        label: "Dropped",
    },
];

/// The member's anime collection, one chunk at a time. Entries come oldest
/// addition first, so an entry added mid-sync lands in the last chunk rather
/// than shifting the earlier ones.
const COLLECTION_QUERY: &str = r#"query ($userId: Int, $userName: String, $status: MediaListStatus, $chunk: Int, $perChunk: Int) {
  MediaListCollection(userId: $userId, userName: $userName, type: ANIME, status: $status, chunk: $chunk, perChunk: $perChunk, forceSingleCompletedList: true, sort: [ADDED_TIME, MEDIA_ID]) {
    hasNextChunk
    user {
      name
      mediaListOptions { animeList { customLists } }
      statistics { anime { count } }
    }
    lists {
      name
      isCustomList
      entries {
        mediaId
        status
        media {
          id
          idMal
          format
          status
          seasonYear
          startDate { year }
          title { userPreferred romaji english }
          genres
          averageScore
        }
      }
    }
  }
}"#;

const VIEWER_QUERY: &str = r#"query {
  Viewer {
    id
    name
    avatar { large medium }
    mediaListOptions { animeList { customLists } }
  }
}"#;

fn list_kinds() -> Vec<ListMediaKind> {
    vec![ListMediaKind::Anime, ListMediaKind::Movie]
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "AniList".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
        sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
        socket_permissions: Vec::new(),
        provider: ProviderDescriptor::ListProvider(ListProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: Vec::new(),
            summary: Some("Your AniList status lists and custom lists".to_string()),
            blurb: Some(
                "Link your AniList account to follow one of your anime lists. Series follow \
                 as anime and films as movies, matched by their AniList and MyAnimeList ids."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#152232".to_string(),
                ink: "#02a9ff".to_string(),
                abbr: "AL".to_string(),
            }),
            brand_url_template: None,
            coverage: list_kinds(),
            auth: ListProviderAuth::MemberAccount {
                // AniList's authorization code grant takes the client secret
                // and documents no PKCE, so the secret stays with the relay
                // (or with the operator's own app).
                flow: ListAccountFlow::AuthorizationCode { pkce: false },
                exchange: ListAccountExchange::SmgRelay,
                byo_app: true,
                // AniList has no scopes: a token can do whatever its member can.
                scopes: Vec::new(),
            },
            groups: vec![ListProviderGroup {
                label: "Your lists".to_string(),
                auth_badge: ListAuthBadge::MemberAccount,
                items: vec![
                    ListProviderItem {
                        id: "status".to_string(),
                        name: "Status list".to_string(),
                        description: Some(
                            "One of your lists by watching status, such as Watching or Planning"
                                .to_string(),
                        ),
                        kinds: list_kinds(),
                        source_type: SOURCE_STATUS.to_string(),
                        params: vec![ListSourceParam {
                            key: PARAM_STATUS.to_string(),
                            label: "Status".to_string(),
                            param_type: ListSourceParamType::Enum,
                            options: STATUSES
                                .iter()
                                .map(|status| status.key.to_string())
                                .collect(),
                            required: true,
                        }],
                        personal: true,
                        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
                    },
                    ListProviderItem {
                        id: "custom-list".to_string(),
                        name: "Custom list".to_string(),
                        description: Some("One of your AniList custom anime lists".to_string()),
                        kinds: list_kinds(),
                        source_type: SOURCE_CUSTOM_LIST.to_string(),
                        params: vec![ListSourceParam {
                            key: PARAM_LIST.to_string(),
                            label: "Custom list name".to_string(),
                            param_type: ListSourceParamType::Text,
                            options: Vec::new(),
                            required: true,
                        }],
                        personal: true,
                        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
                    },
                ],
            }],
            notes: vec![ListProviderNote {
                tone: ListNoteTone::Info,
                text_key: "lists.note.anilist_collection_cap".to_string(),
            }],
            url_patterns: Vec::new(),
            capabilities: ListProviderCapabilities {
                account: true,
                health: false,
                requires_member_credential: true,
            },
            config_fields: Vec::new(),
            default_base_url: None,
            allowed_hosts: vec![API_HOST.to_string()],
            rate_limit_seconds: Some(RATE_LIMIT_SECONDS),
        }),
    }
}

async fn handle_command(command: PluginListCommand) -> PluginListCommandResult {
    run(&HostHttp, command).await
}

pub async fn run<H: ListHttp>(http: &H, command: PluginListCommand) -> PluginListCommandResult {
    match command {
        PluginListCommand::Fetch(request) => {
            PluginListCommandResult::Fetch(into_result(fetch(http, &request).await))
        }
        PluginListCommand::Account(request) => {
            PluginListCommandResult::Account(into_result(account(http, &request.credential).await))
        }
        PluginListCommand::Health(_) => {
            PluginListCommandResult::Health(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "AniList lists use each member's account and have no server key to check",
            )))
        }
    }
}

fn into_result<T>(result: Result<T, PluginError>) -> PluginResult<T> {
    match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => PluginResult::Err(error),
    }
}

/// What one subscription follows.
enum Source {
    Status(&'static Status),
    Custom(String),
}

impl Source {
    fn from_request(request: &ListPluginFetchRequest) -> Result<Self, PluginError> {
        match request.source_type.as_str() {
            SOURCE_STATUS => {
                let value = required_param(request, PARAM_STATUS)?;
                STATUSES
                    .iter()
                    .find(|status| {
                        status.key.eq_ignore_ascii_case(value)
                            || status.anilist.eq_ignore_ascii_case(value)
                    })
                    .map(Source::Status)
                    .ok_or_else(|| invalid_config(format!("unknown AniList list status {value}")))
            }
            SOURCE_CUSTOM_LIST => Ok(Source::Custom(
                required_param(request, PARAM_LIST)?.to_string(),
            )),
            other => Err(unsupported_source(other)),
        }
    }

    /// Names the list in a not-found message without naming the member's
    /// own list.
    fn what(&self) -> &'static str {
        match self {
            Source::Status(_) => "AniList list",
            Source::Custom(_) => "AniList custom list",
        }
    }
}

fn required_param<'a>(
    request: &'a ListPluginFetchRequest,
    key: &str,
) -> Result<&'a str, PluginError> {
    request
        .params
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_param(key))
}

/// The AniList member a collection query names.
enum Member {
    Id(u64),
    Name(String),
}

impl Member {
    fn from_credential(credential: &ListCredential) -> Result<Self, PluginError> {
        if let Some(id) = credential
            .external_user_id
            .as_deref()
            .and_then(|id| id.trim().parse::<u64>().ok())
            .filter(|id| *id > 0)
        {
            return Ok(Member::Id(id));
        }
        credential
            .username
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| Member::Name(name.to_string()))
            .ok_or_else(|| {
                auth_failed("the linked AniList account has no AniList user; reconnect it")
            })
    }

    fn variables(&self) -> Map<String, Value> {
        let mut variables = Map::new();
        match self {
            Member::Id(id) => variables.insert("userId".to_string(), Value::from(*id)),
            Member::Name(name) => {
                variables.insert("userName".to_string(), Value::from(name.as_str()))
            }
        };
        variables
    }
}

fn account_required() -> PluginError {
    auth_failed("an AniList list needs a linked AniList account")
}

fn rejected_token() -> PluginError {
    auth_failed("AniList no longer accepts the linked account; reconnect it")
}

/// The member's access token. AniList issues JWTs, so a value with
/// whitespace or control characters cannot be one and is never sent.
fn bearer_token(credential: &ListCredential) -> Result<&str, PluginError> {
    let token = credential.access_token.trim();
    if token.is_empty()
        || token
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(auth_failed(
            "the linked AniList account has no usable access token; reconnect it",
        ));
    }
    Ok(token)
}

/// One GraphQL POST. The token travels in the `Authorization` header only,
/// never in the URL or the body.
fn graphql_request(token: &str, query: &str, variables: Map<String, Value>) -> PluginHttpRequest {
    let mut request = get(API_URL, USER_AGENT, ACCEPT);
    request.method = Some("POST".to_string());
    request
        .headers
        .insert("Content-Type".to_string(), "application/json".to_string());
    request
        .headers
        .insert("Authorization".to_string(), format!("Bearer {token}"));
    request.body = json!({ "query": query, "variables": variables })
        .to_string()
        .into_bytes();
    request
}

async fn graphql<H: ListHttp>(
    http: &H,
    token: &str,
    query: &str,
    variables: Map<String, Value>,
    what: &str,
) -> Result<Value, PluginError> {
    let response = http.send(graphql_request(token, query, variables)).await?;
    check_response(&response, what)
}

/// Map an AniList answer onto the host's failure classes. AniList reports
/// most failures in the GraphQL `errors` array, sometimes alongside HTTP 200,
/// so the array is read whatever the status. AniList's own messages are never
/// passed on, because they can quote the request.
fn check_response(response: &PluginHttpResponse, what: &str) -> Result<Value, PluginError> {
    let status = response.status;
    match status {
        401 => return Err(rejected_token()),
        429 => return Err(rate_limited(retry_after_seconds(response))),
        // AniList answers 403 while its API is switched off for maintenance.
        403 => {
            return Err(unavailable(
                "AniList refused the request (HTTP 403); its API may be switched off",
            ));
        }
        408 | 500..=599 => {
            return Err(unavailable(format!(
                "AniList is unavailable (HTTP {status})"
            )));
        }
        _ => {}
    }
    let body = serde_json::from_slice::<Value>(&response.body).ok();
    if let Some(errors) = body
        .as_ref()
        .and_then(|body| body.get("errors"))
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        return Err(graphql_error(errors, response, what));
    }
    match status {
        200..=299 => body.ok_or_else(|| permanent("the AniList response is not valid JSON")),
        404 | 410 => Err(not_found(format!("{what} (HTTP {status})"))),
        _ => Err(permanent(format!(
            "AniList answered with unexpected HTTP {status}"
        ))),
    }
}

/// Classify a GraphQL `errors` array by its most telling entry: a rejected
/// token, then a rate limit, then a missing member or list, then an outage.
fn graphql_error(errors: &[Value], response: &PluginHttpResponse, what: &str) -> PluginError {
    let reported: Vec<(i64, String)> = errors
        .iter()
        .map(|error| {
            let status = error
                .get("status")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            (status, message)
        })
        .collect();
    let any = |test: &dyn Fn(i64, &str) -> bool| {
        reported
            .iter()
            .any(|(status, message)| test(*status, message))
    };
    if any(&|status, message| {
        status == 401
            || message.contains("invalid token")
            || message.contains("unauthorized")
            || message.contains("unauthenticated")
    }) {
        return rejected_token();
    }
    if any(&|status, message| status == 429 || message.contains("too many requests")) {
        return rate_limited(retry_after_seconds(response));
    }
    if any(&|status, message| status == 404 || message.contains("not found")) {
        return not_found(what);
    }
    if any(&|status, _| status == 403 || status == 408 || status >= 500) {
        return unavailable("AniList could not answer the request right now");
    }
    match reported.iter().map(|(status, _)| *status).find(|s| *s > 0) {
        Some(status) => permanent(format!("AniList rejected the request (status {status})")),
        None => permanent("AniList rejected the request"),
    }
}

pub async fn fetch<H: ListHttp>(
    http: &H,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    let source = Source::from_request(request)?;
    let credential = request.credential.as_ref().ok_or_else(account_required)?;
    let token = bearer_token(credential)?;
    let member = Member::from_credential(credential)?;
    let chunk = numeric_cursor(request.page_cursor.as_deref(), 1)?;
    if !(1..=MAX_CHUNKS).contains(&chunk) {
        return Err(permanent(format!("invalid page cursor {chunk}")));
    }

    let mut variables = member.variables();
    variables.insert("chunk".to_string(), Value::from(chunk));
    variables.insert("perChunk".to_string(), Value::from(PER_CHUNK));
    if let Source::Status(status) = &source {
        variables.insert("status".to_string(), Value::from(status.anilist));
    }
    let body = graphql(http, token, COLLECTION_QUERY, variables, source.what()).await?;
    let collection = body
        .pointer("/data/MediaListCollection")
        .filter(|collection| collection.is_object())
        .ok_or_else(|| permanent("AniList returned no list collection"))?;
    let user = collection.get("user");
    let custom_lists = custom_list_names(user);
    let groups = collection
        .get("lists")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let in_chunk = distinct_entries(groups);
    let has_next = match collection.get("hasNextChunk").and_then(Value::as_bool) {
        Some(has_next) => has_next,
        // Without the flag a full chunk may have more after it, and ending
        // the list here would read every later entry as having left it.
        None if in_chunk >= PER_CHUNK => {
            return Err(permanent(
                "AniList answered a full chunk without saying whether another follows, so \
                 Scryer cannot tell whether the list is complete",
            ));
        }
        None => false,
    };

    // The cap applies to the whole collection, before a status filter, so a
    // member past it may be missing entries from any status list.
    let total = user
        .and_then(|user| user.pointer("/statistics/anime/count"))
        .and_then(Value::as_u64);
    let read_so_far = ((chunk - 1) * PER_CHUNK).saturating_add(in_chunk);
    if total.is_some_and(|total| total >= u64::from(COLLECTION_CAP))
        || read_so_far >= COLLECTION_CAP
        || (has_next && chunk >= MAX_CHUNKS)
    {
        return Err(permanent(
            "this AniList collection reached AniList's 11,000-entry limit, so Scryer cannot \
             read all of it",
        ));
    }

    let (entries, list_name) = match &source {
        Source::Status(status) => (status_entries(groups, status), status.label.to_string()),
        Source::Custom(name) => {
            let known = custom_lists.as_deref().map(|names| matching(names, name));
            // AniList lists the member's custom list names on every chunk, so
            // a name it does not know is a list that is gone, while a known
            // list with no entries here is simply empty.
            if known.as_ref().is_some_and(Vec::is_empty) {
                return Err(not_found(source.what()));
            }
            let list_name = known
                .and_then(|names| names.first().map(|name| name.to_string()))
                .unwrap_or_else(|| name.clone());
            (custom_entries(groups, name), list_name)
        }
    };
    let items: Vec<ListPluginItem> = entries.into_iter().filter_map(entry_item).collect();
    let list_url = list_url(user, credential, &list_name);

    if chunk == 1 && !has_next {
        return Ok(single_page(
            dedupe_and_rank(items, 1),
            Some(list_name),
            list_url,
            request.since_fingerprint.as_deref(),
        ));
    }
    Ok(ListPluginFetchResponse {
        items: dedupe_and_rank(items, (chunk - 1) * PER_CHUNK + 1),
        next_cursor: has_next.then(|| (chunk + 1).to_string()),
        list_name: Some(list_name),
        list_url,
        total_hint: None,
        // A chunked read carries no fingerprint: the host compares only the
        // first page, which cannot vouch for the rest.
        fingerprint: None,
        unchanged: false,
    })
}

fn is_custom(group: &Value) -> bool {
    group
        .get("isCustomList")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn group_entries(group: &Value) -> impl Iterator<Item = &Value> {
    group
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn entry_media_id(entry: &Value) -> Option<String> {
    json_id(entry.pointer("/media/id")).or_else(|| json_id(entry.get("mediaId")))
}

/// Distinct entries in this chunk. An entry shows up once per list that
/// holds it, so the groups overlap.
fn distinct_entries(groups: &[Value]) -> u32 {
    let ids: BTreeSet<String> = groups
        .iter()
        .flat_map(group_entries)
        .filter_map(entry_media_id)
        .collect();
    u32::try_from(ids.len()).unwrap_or(u32::MAX)
}

/// Every entry with the status, status lists first. Custom lists come after
/// because they also hold entries the member hid from the status lists.
fn status_entries<'a>(groups: &'a [Value], status: &Status) -> Vec<&'a Value> {
    let status_groups = groups.iter().filter(|group| !is_custom(group));
    let custom_groups = groups.iter().filter(|group| is_custom(group));
    status_groups
        .chain(custom_groups)
        .flat_map(group_entries)
        .filter(|entry| {
            entry
                .get("status")
                .and_then(Value::as_str)
                .is_none_or(|value| value.eq_ignore_ascii_case(status.anilist))
        })
        .collect()
}

fn custom_entries<'a>(groups: &'a [Value], name: &str) -> Vec<&'a Value> {
    let custom: Vec<&Value> = groups.iter().filter(|group| is_custom(group)).collect();
    let names: Vec<&str> = custom
        .iter()
        .filter_map(|group| group.get("name").and_then(Value::as_str))
        .collect();
    let wanted = matching(&names, name);
    custom
        .into_iter()
        .filter(|group| {
            group
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|group_name| wanted.contains(&group_name))
        })
        .flat_map(group_entries)
        .collect()
}

/// The names that are the wanted list: an exact match when there is one,
/// otherwise a match that ignores case and surrounding space.
fn matching<'a, S: AsRef<str>>(names: &'a [S], wanted: &str) -> Vec<&'a str> {
    let wanted = wanted.trim();
    let exact: Vec<&str> = names
        .iter()
        .map(AsRef::as_ref)
        .filter(|name| name.trim() == wanted)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let folded = wanted.to_lowercase();
    names
        .iter()
        .map(AsRef::as_ref)
        .filter(|name| name.trim().to_lowercase() == folded)
        .collect()
}

/// The member's custom anime list names, when AniList included them.
fn custom_list_names(user: Option<&Value>) -> Option<Vec<String>> {
    let names = user?
        .pointer("/mediaListOptions/animeList/customLists")?
        .as_array()?;
    let mut seen = BTreeSet::new();
    Some(
        names
            .iter()
            .filter_map(|name| name.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty() && seen.insert(name.to_string()))
            .map(str::to_string)
            .collect(),
    )
}

/// The list on AniList's site. Only the member who follows a personal list
/// ever sees it.
fn list_url(user: Option<&Value>, credential: &ListCredential, list_name: &str) -> Option<String> {
    let name = json_text(user.and_then(|user| user.get("name"))).or_else(|| {
        credential
            .username
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })?;
    Some(format!(
        "{SITE_BASE}/user/{}/animelist/{}",
        encode_component(&name),
        encode_component(list_name)
    ))
}

/// AniList films route to movies; series, shorts, OVAs, ONAs, specials and
/// music videos route to anime, and so does an entry with no format.
pub fn kind_for_format(format: Option<&str>) -> ListMediaKind {
    match format {
        Some(format) if format.eq_ignore_ascii_case("MOVIE") => ListMediaKind::Movie,
        _ => ListMediaKind::Anime,
    }
}

/// Whether anything of the title has aired. A cancelled title may never have
/// aired, so it stays unknown.
fn released(status: Option<&str>) -> Option<bool> {
    match status? {
        "FINISHED" | "RELEASING" | "HIATUS" => Some(true),
        "NOT_YET_RELEASED" => Some(false),
        _ => None,
    }
}

fn entry_item(entry: &Value) -> Option<ListPluginItem> {
    let media = entry.get("media").filter(|media| media.is_object());
    let field = |key: &str| media.and_then(|media| media.get(key));
    let anilist_id = entry_media_id(entry)?;
    let format = json_text(field("format")).map(|format| format.to_ascii_uppercase());
    let kind = kind_for_format(format.as_deref());
    let title = field("title").and_then(|title| {
        ["userPreferred", "romaji", "english"]
            .iter()
            .find_map(|key| json_text(title.get(*key)))
    });
    let year = json_year(field("startDate").and_then(|date| date.get("year")))
        .or_else(|| json_year(field("seasonYear")));

    let mut external_ids = vec![ListExternalId {
        source: ID_ANILIST.to_string(),
        kind: None,
        id: anilist_id.clone(),
    }];
    if let Some(mal) = json_id(field("idMal")) {
        external_ids.push(ListExternalId {
            source: ID_MAL.to_string(),
            kind: None,
            id: mal,
        });
    }
    let genres = field("genres")
        .and_then(Value::as_array)
        .map(|genres| {
            genres
                .iter()
                .filter_map(|genre| json_text(Some(genre)))
                .collect()
        })
        .unwrap_or_default();
    let provider_rating = field("averageScore")
        .and_then(Value::as_f64)
        .filter(|score| *score > 0.0)
        .map(|value| ListProviderRating {
            scale: PROVIDER_TYPE.to_string(),
            value,
        });

    Some(ListPluginItem {
        item_key: format!("{ID_ANILIST}:{anilist_id}"),
        kind_hint: Some(kind),
        title,
        year,
        external_ids,
        provider_rating,
        genres,
        format,
        released: released(json_text(field("status")).as_deref()),
        ..ListPluginItem::default()
    })
}

pub async fn account<H: ListHttp>(
    http: &H,
    credential: &ListCredential,
) -> Result<ListPluginAccountResponse, PluginError> {
    let token = bearer_token(credential)?;
    let body = graphql(http, token, VIEWER_QUERY, Map::new(), "AniList account").await?;
    // AniList names no viewer only when it does not accept the token.
    let viewer = body
        .pointer("/data/Viewer")
        .filter(|viewer| viewer.is_object())
        .ok_or_else(rejected_token)?;
    let external_user_id = json_id(viewer.get("id"))
        .ok_or_else(|| permanent("AniList returned an account without an id"))?;
    let username = json_text(viewer.get("name"))
        .ok_or_else(|| permanent("AniList returned an account without a name"))?;
    let avatar_url = viewer.get("avatar").and_then(|avatar| {
        json_text(avatar.get("large"))
            .or_else(|| json_text(avatar.get("medium")))
            .filter(|url| url.starts_with("https://"))
    });
    let owned_lists = custom_list_names(Some(viewer))
        .unwrap_or_default()
        .into_iter()
        .map(|name| ListAccountList {
            id: name.clone(),
            name,
            kinds: list_kinds(),
        })
        .collect();
    let statuses = STATUSES
        .iter()
        .map(|status| ListAccountStatus {
            key: status.key.to_string(),
            label: status.label.to_string(),
            kinds: list_kinds(),
        })
        .collect();
    Ok(ListPluginAccountResponse {
        external_user_id,
        display_name: Some(username.clone()),
        username,
        avatar_url,
        owned_lists,
        statuses,
    })
}

#[cfg(test)]
mod tests;
