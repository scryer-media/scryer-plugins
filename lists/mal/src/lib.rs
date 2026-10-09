//! MyAnimeList list provider.
//!
//! Follows a linked member's own anime status lists (watching, completed, on
//! hold, dropped and plan to watch) through MyAnimeList API v2, sending the
//! member's OAuth access token as a bearer token. MyAnimeList's public
//! rankings and seasonal charts are served by the metadata gateway and are
//! deliberately absent here.
//!
//! MyAnimeList's API agreement does not allow personal information or
//! member-generated content to be kept server-side, so each entry is reduced
//! to what matching and list filters need: the MyAnimeList id, the kind, the
//! format, the start year and catalog facts such as the airing state. The
//! member's own scores, progress, dates, tags and comments are never
//! requested, titles are not passed on because the id resolves the entry, and
//! no username goes into a list name or address.
//!
//! The OAuth client id belongs to the host's account-link flow. Every request
//! here carries the member's token, which MyAnimeList accepts without a
//! client id header.

use list_provider_common::error::{
    Access, auth_failed, check_status, permanent, plugin_error, unavailable, unsupported_source,
};
use list_provider_common::http::{HostHttp, ListHttp, get, json_body};
use list_provider_common::ids::{dedupe_and_rank, json_id, json_text, json_year};
use list_provider_common::page::{numeric_cursor, single_page};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{
    ListAccountExchange, ListAccountFlow, ListAccountStatus, ListAuthBadge, ListCredential,
    ListExternalId, ListMediaKind, ListPluginAccountResponse, ListPluginFetchRequest,
    ListPluginFetchResponse, ListPluginItem, ListProviderAuth, ListProviderCapabilities,
    ListProviderDescriptor, ListProviderGroup, ListProviderItem, ListProviderRating,
    ListProviderTile, PluginDescriptor, PluginError, PluginErrorCode, PluginResult,
    ProviderDescriptor,
};
use serde_json::Value;

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/runtime-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/list-v1.0.0"],
    generate_all,
});

list_provider_common::list_component_main!(descriptor = descriptor, handler = handle_command,);

pub const PLUGIN_ID: &str = "mal-list";
pub const PROVIDER_TYPE: &str = "mal";
/// Source types are `status:` followed by a MyAnimeList list status.
pub const STATUS_SOURCE_PREFIX: &str = "status:";
/// MyAnimeList's list statuses, in MyAnimeList's own order, with the label
/// each is shown under.
pub const STATUSES: [(&str, &str); 5] = [
    ("watching", "Watching"),
    ("completed", "Completed"),
    ("on_hold", "On Hold"),
    ("dropped", "Dropped"),
    ("plan_to_watch", "Plan to Watch"),
];
/// The OAuth scope MyAnimeList documents for member calls. It is the only
/// scope MyAnimeList defines; there is no read-only one.
pub const SCOPE: &str = "write:users";

pub const API_BASE: &str = "https://api.myanimelist.net/v2";
const API_HOST: &str = "api.myanimelist.net";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/json";
/// The external id source the metadata gateway maps through its anime id
/// tables, and the kind Scryer files MyAnimeList ids under.
const ID_SOURCE: &str = "mal";
const ID_KIND: &str = "anime";
/// Catalog fields read for every entry. The member's own list status is
/// never requested.
const ANIME_FIELDS: &str = "media_type,start_date,start_season,status,mean,genres";
/// MyAnimeList's largest page.
pub const PAGE_LIMIT: u32 = 1000;
/// Deepest page followed: 20,000 entries, well inside the host's
/// hundred-page ceiling per sync.
pub const MAX_PAGES: u32 = 20;
/// Six hours, Sonarr's shortest refresh for a MyAnimeList list.
const DEFAULT_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
/// MyAnimeList documents no rate limit, so requests keep the arrs' default
/// two-second spacing for list requests.
const RATE_LIMIT_SECONDS: i64 = 2;

fn kinds() -> Vec<ListMediaKind> {
    vec![ListMediaKind::Anime, ListMediaKind::Movie]
}

fn status_description(status: &str) -> &'static str {
    match status {
        "watching" => "Anime you are watching now",
        "completed" => "Anime you have finished",
        "on_hold" => "Anime you have put on hold",
        "dropped" => "Anime you have dropped",
        _ => "Anime you plan to watch",
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "MyAnimeList".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
        sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
        socket_permissions: Vec::new(),
        provider: ProviderDescriptor::ListProvider(ListProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec!["myanimelist".to_string()],
            summary: Some("Your MyAnimeList anime status lists".to_string()),
            blurb: Some(
                "Link your MyAnimeList account to follow your own status lists, such as \
                 Plan to Watch. Rankings and seasonal charts come from Scryer's metadata \
                 service instead."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#2e51a2".to_string(),
                ink: "#ffffff".to_string(),
                abbr: "MAL".to_string(),
            }),
            brand_url_template: None,
            coverage: kinds(),
            auth: ListProviderAuth::MemberAccount {
                flow: ListAccountFlow::AuthorizationCode { pkce: true },
                exchange: ListAccountExchange::SmgRelay,
                byo_app: true,
                scopes: vec![SCOPE.to_string()],
            },
            groups: vec![ListProviderGroup {
                label: "Your account".to_string(),
                auth_badge: ListAuthBadge::MemberAccount,
                items: STATUSES
                    .iter()
                    .map(|(status, label)| ListProviderItem {
                        id: status.replace('_', "-"),
                        name: label.to_string(),
                        description: Some(status_description(status).to_string()),
                        kinds: kinds(),
                        source_type: format!("{STATUS_SOURCE_PREFIX}{status}"),
                        params: Vec::new(),
                        personal: true,
                        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
                    })
                    .collect(),
            }],
            notes: Vec::new(),
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
                "MyAnimeList has no server-wide key to check",
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

/// The member's access token. A missing or malformed one asks for the
/// account to be linked again; the token itself never appears in a message.
fn access_token(credential: Option<&ListCredential>) -> Result<&str, PluginError> {
    let token = credential
        .map(|credential| credential.access_token.trim())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| auth_failed("a linked MyAnimeList account is required"))?;
    if token
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(auth_failed(
            "the linked MyAnimeList account has an unusable token",
        ));
    }
    Ok(token)
}

fn authorized_get(url: String, token: &str) -> PluginHttpRequest {
    let mut request = get(url, USER_AGENT, ACCEPT);
    request
        .headers
        .insert("Authorization".to_string(), format!("Bearer {token}"));
    request
}

/// MyAnimeList answers an expired or invalid token with 401. It documents 403
/// as "DoS detected etc." and also answers 403 to a request that carries no
/// client credentials, so 403 is MyAnimeList refusing the request for now,
/// never a sign that the list is gone.
fn check_mal_status(response: &PluginHttpResponse, what: &str) -> Result<(), PluginError> {
    match response.status {
        401 => Err(auth_failed(
            "MyAnimeList no longer accepts the linked account (HTTP 401)",
        )),
        403 => Err(unavailable("MyAnimeList refused the request (HTTP 403)")),
        _ => check_status(response, Access::ServerKey, what),
    }
}

/// The MyAnimeList status and label a source type names.
fn status_of(source_type: &str) -> Result<(&'static str, &'static str), PluginError> {
    source_type
        .strip_prefix(STATUS_SOURCE_PREFIX)
        .and_then(|status| STATUSES.iter().find(|(known, _)| *known == status))
        .copied()
        .ok_or_else(|| unsupported_source(source_type))
}

/// One page of a status list. `nsfw=true` keeps adult entries in it:
/// MyAnimeList leaves them out by default, and a list missing them would make
/// them look like they left.
fn list_request_url(status: &str, offset: u32) -> String {
    format!(
        "{API_BASE}/users/@me/animelist?status={status}&sort=anime_title&fields={ANIME_FIELDS}&limit={PAGE_LIMIT}&offset={offset}&nsfw=true"
    )
}

pub async fn fetch<H: ListHttp>(
    http: &H,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    let (status, label) = status_of(&request.source_type)?;
    let token = access_token(request.credential.as_ref())?;
    let offset = numeric_cursor(request.page_cursor.as_deref(), 0)?;
    if offset >= PAGE_LIMIT * MAX_PAGES {
        return Err(permanent(format!("invalid page cursor {offset}")));
    }
    let response = http
        .send(authorized_get(list_request_url(status, offset), token))
        .await?;
    check_mal_status(&response, &format!("MyAnimeList {label} list"))?;
    let body = json_body(&response)?;
    // A response without its entry array is malformed, never an empty list:
    // treating it as empty would make every title look like it left.
    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| permanent("the MyAnimeList list response has no entries"))?;
    let items: Vec<ListPluginItem> = entries.iter().filter_map(to_item).collect();
    let next = next_offset(&body, offset, entries.len())?;
    let list_name = Some(format!("MyAnimeList {label}"));
    if offset == 0 && next.is_none() {
        return Ok(single_page(
            dedupe_and_rank(items, 1),
            list_name,
            None,
            request.since_fingerprint.as_deref(),
        ));
    }
    Ok(ListPluginFetchResponse {
        items: dedupe_and_rank(items, offset + 1),
        next_cursor: next.map(|next| next.to_string()),
        list_name,
        list_url: None,
        total_hint: None,
        // Multi-page lists carry no fingerprint: the host only compares the
        // first page, which cannot vouch for the rest.
        fingerprint: None,
        unchanged: false,
    })
}

/// The offset of the next page when MyAnimeList says there is one. Only the
/// offset of MyAnimeList's `paging.next` address is used, so every request
/// stays on the API host with this plugin's own parameters. That offset must
/// move past this page without skipping any of it: one that stays put, goes
/// back, or jumps beyond the entries just read fails, since following it
/// would repeat or silently drop entries. A list longer than the page cap
/// fails rather than being cut short, because a truncated list would make
/// its tail look like it left.
fn next_offset(body: &Value, offset: u32, read: usize) -> Result<Option<u32>, PluginError> {
    let Some(next) = body
        .get("paging")
        .and_then(|paging| paging.get("next"))
        .and_then(Value::as_str)
        .filter(|next| !next.trim().is_empty())
    else {
        return Ok(None);
    };
    let read = u32::try_from(read).unwrap_or(u32::MAX);
    let next = query_offset(next)
        .filter(|next| *next > offset && *next <= offset.saturating_add(read))
        .ok_or_else(|| {
            permanent("the MyAnimeList list gave a next page that does not follow this one")
        })?;
    if next >= PAGE_LIMIT * MAX_PAGES {
        return Err(permanent(format!(
            "the MyAnimeList list has more than {} entries, more than Scryer follows",
            PAGE_LIMIT * MAX_PAGES
        )));
    }
    Ok(Some(next))
}

fn query_offset(url: &str) -> Option<u32> {
    let (_, query) = url.split_once('?')?;
    query
        .split('#')
        .next()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("offset="))
        .and_then(|value| value.parse().ok())
}

/// How a MyAnimeList media type routes, and the format it is filed under.
/// Films are movies; every other screen format is an anime series, keeping
/// MyAnimeList's own name for the format. Music videos are not titles Scryer
/// manages and are skipped. MyAnimeList's site also files commercials (CM)
/// and promotional videos (PV) as anime; its API documents no value for
/// them, so `cm` and `pv` are skipped too and any other value routes as anime.
pub fn media_kind(media_type: &str) -> Option<(ListMediaKind, Option<String>)> {
    match media_type {
        "movie" => Some((ListMediaKind::Movie, Some("movie".to_string()))),
        "music" | "cm" | "pv" => None,
        "" | "unknown" => Some((ListMediaKind::Anime, None)),
        other => Some((ListMediaKind::Anime, Some(other.to_string()))),
    }
}

/// Map one list entry. Entries without an id, and media types Scryer does
/// not manage, are skipped.
fn to_item(entry: &Value) -> Option<ListPluginItem> {
    let node = entry.get("node")?;
    let id = json_id(node.get("id"))?;
    let media_type = node
        .get("media_type")
        .and_then(Value::as_str)
        .map(|media_type| media_type.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let (kind, format) = media_kind(&media_type)?;
    let year = json_year(node.get("start_date")).or_else(|| {
        json_year(
            node.get("start_season")
                .and_then(|season| season.get("year")),
        )
    });
    let released = match node.get("status").and_then(Value::as_str) {
        Some("not_yet_aired") => Some(false),
        Some("finished_airing" | "currently_airing") => Some(true),
        _ => None,
    };
    let provider_rating = node
        .get("mean")
        .and_then(Value::as_f64)
        .filter(|mean| *mean > 0.0)
        .map(|value| ListProviderRating {
            scale: PROVIDER_TYPE.to_string(),
            value,
        });
    let genres = node
        .get("genres")
        .and_then(Value::as_array)
        .map(|genres| {
            genres
                .iter()
                .filter_map(|genre| json_text(genre.get("name")))
                .collect()
        })
        .unwrap_or_default();
    Some(ListPluginItem {
        item_key: format!("{ID_SOURCE}:{id}"),
        kind_hint: Some(kind),
        year,
        external_ids: vec![ListExternalId {
            source: ID_SOURCE.to_string(),
            kind: Some(ID_KIND.to_string()),
            id,
        }],
        provider_rating,
        genres,
        format,
        released,
        ..ListPluginItem::default()
    })
}

fn account_statuses() -> Vec<ListAccountStatus> {
    STATUSES
        .iter()
        .map(|(status, label)| ListAccountStatus {
            key: format!("{STATUS_SOURCE_PREFIX}{status}"),
            label: label.to_string(),
            kinds: kinds(),
        })
        .collect()
}

/// The identity behind a member token, and the statuses it can follow.
pub async fn account<H: ListHttp>(
    http: &H,
    credential: &ListCredential,
) -> Result<ListPluginAccountResponse, PluginError> {
    let token = access_token(Some(credential))?;
    let response = http
        .send(authorized_get(format!("{API_BASE}/users/@me"), token))
        .await?;
    check_mal_status(&response, "MyAnimeList account")?;
    let body = json_body(&response)?;
    let external_user_id = json_id(body.get("id"))
        .ok_or_else(|| permanent("the MyAnimeList account response has no user id"))?;
    let username = json_text(body.get("name"))
        .ok_or_else(|| permanent("the MyAnimeList account response has no user name"))?;
    let avatar_url = json_text(body.get("picture")).filter(|url| url.starts_with("https://"));
    Ok(ListPluginAccountResponse {
        external_user_id,
        username,
        display_name: None,
        avatar_url,
        owned_lists: Vec::new(),
        statuses: account_statuses(),
    })
}

#[cfg(test)]
mod tests;
