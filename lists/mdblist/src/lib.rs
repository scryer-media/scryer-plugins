//! MDBList list provider.
//!
//! Follows any MDBList list by its address (`mdblist.com/lists/<user>/<slug>`)
//! or, with the server's MDBList API key, by its numeric id. Without a key the
//! plugin reads the list's public JSON export in one request; with a key it
//! reads the API's items endpoint, which also reaches the key owner's private
//! lists and pages through lists longer than one response.
//!
//! The key is server configuration declared in the descriptor; no member
//! credential is ever read or stored. MDBList's directory of top lists is not
//! a list itself, so it is not a source here: any top list is followed by its
//! address like every other list.

use list_provider_common::error::{
    Access, check_status, invalid_config, missing_param, plugin_error, unsupported_source,
};
use list_provider_common::http::{HostHttp, ListHttp, encode_component, get, header, json_body};
use list_provider_common::ids::{Ids, build_item, dedupe_and_rank, json_id, json_text, json_year};
use list_provider_common::page::{numeric_cursor, single_page};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ListAuthBadge, ListMediaKind, ListNoteTone,
    ListPluginFetchRequest, ListPluginFetchResponse, ListPluginHealthResponse, ListPluginItem,
    ListProviderAuth, ListProviderCapabilities, ListProviderDescriptor, ListProviderGroup,
    ListProviderItem, ListProviderNote, ListProviderTile, ListSourceParam, ListSourceParamType,
    ListUrlPattern, ListUrlPatternCapture, PluginDescriptor, PluginError, PluginErrorCode,
    PluginResult, ProviderDescriptor,
};
use serde_json::Value;

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["wit/host-v1.0.0", "wit/runtime-v1.0.0", "wit/list-v1.0.0"],
    generate_all,
});

list_provider_common::list_component_main!(descriptor = descriptor, handler = handle_command,);

pub const PLUGIN_ID: &str = "mdblist-list";
pub const PROVIDER_TYPE: &str = "mdblist";
pub const CONFIG_API_KEY: &str = "api_key";
pub const SOURCE_LIST: &str = "list";
pub const PARAM_LIST: &str = "list";

pub const API_BASE: &str = "https://api.mdblist.com";
pub const SITE_BASE: &str = "https://mdblist.com";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/json";
/// Items per API page; MDBList's documented default and a single request for
/// almost every list.
pub const PAGE_LIMIT: u32 = 1000;
/// Deepest API page followed, far inside the host's per-sync page ceiling.
const MAX_PAGES: u32 = 20;
/// Twelve hours, the interval the other arrs use for MDBList.
const DEFAULT_INTERVAL_SECONDS: u64 = 12 * 60 * 60;

pub fn descriptor() -> PluginDescriptor {
    let both = vec![ListMediaKind::Movie, ListMediaKind::Series];
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "MDBList".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
        sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
        socket_permissions: Vec::new(),
        provider: ProviderDescriptor::ListProvider(ListProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: Vec::new(),
            summary: Some("Any MDBList list, static or dynamic".to_string()),
            blurb: Some(
                "Paste an MDBList list address. Public lists need no key; an MDBList API \
                 key in the provider settings also reaches that account's private lists \
                 and numeric list ids."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#1b2433".to_string(),
                ink: "#f5c518".to_string(),
                abbr: "MD".to_string(),
            }),
            brand_url_template: Some(format!("{SITE_BASE}/lists/{{{PARAM_LIST}}}")),
            coverage: both.clone(),
            auth: ListProviderAuth::ServerApiKey {
                config_field: CONFIG_API_KEY.to_string(),
            },
            groups: vec![ListProviderGroup {
                label: "Lists".to_string(),
                auth_badge: ListAuthBadge::NoAccountNeedsValue,
                items: vec![ListProviderItem {
                    id: "list".to_string(),
                    name: "List".to_string(),
                    description: Some(
                        "Any public list by its address, or a numeric id with the API key"
                            .to_string(),
                    ),
                    kinds: both,
                    source_type: SOURCE_LIST.to_string(),
                    params: vec![ListSourceParam {
                        key: PARAM_LIST.to_string(),
                        label: "List (user/slug or id)".to_string(),
                        param_type: ListSourceParamType::Text,
                        options: Vec::new(),
                        required: true,
                    }],
                    personal: false,
                    default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
                }],
            }],
            notes: vec![ListProviderNote {
                tone: ListNoteTone::Info,
                text_key: "lists.note.mdblist_dynamic".to_string(),
            }],
            url_patterns: vec![ListUrlPattern {
                pattern: r"^https?://(?:www\.)?mdblist\.com/lists/(?<list>[^/?#]+/[^/?#]+)(?:/json)?/?(?:[?#].*)?$"
                    .to_string(),
                source_type: SOURCE_LIST.to_string(),
                captures: vec![ListUrlPatternCapture {
                    group: "list".to_string(),
                    param: PARAM_LIST.to_string(),
                }],
            }],
            capabilities: ListProviderCapabilities {
                account: false,
                health: true,
                requires_member_credential: false,
            },
            config_fields: vec![ConfigFieldDef {
                key: CONFIG_API_KEY.to_string(),
                label: "MDBList API key".to_string(),
                field_type: ConfigFieldType::Password,
                required: false,
                help_text: Some(
                    "Optional. Needed for private lists and numeric list ids.".to_string(),
                ),
                ..Default::default()
            }],
            default_base_url: None,
            allowed_hosts: vec!["api.mdblist.com".to_string(), "mdblist.com".to_string()],
            rate_limit_seconds: Some(1),
        }),
    }
}

async fn handle_command(command: PluginListCommand) -> PluginListCommandResult {
    let api_key = scryer_plugin_pdk::config::get(CONFIG_API_KEY)
        .ok()
        .flatten();
    run(&HostHttp, api_key.as_deref(), command).await
}

fn into_result<T>(result: Result<T, PluginError>) -> PluginResult<T> {
    match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => PluginResult::Err(error),
    }
}

pub async fn run<H: ListHttp>(
    http: &H,
    api_key: Option<&str>,
    command: PluginListCommand,
) -> PluginListCommandResult {
    let api_key = api_key.map(str::trim).filter(|key| !key.is_empty());
    match command {
        PluginListCommand::Fetch(request) => {
            PluginListCommandResult::Fetch(into_result(fetch(http, api_key, &request).await))
        }
        PluginListCommand::Health(_) => {
            PluginListCommandResult::Health(into_result(health(http, api_key).await))
        }
        PluginListCommand::Account(_) => {
            PluginListCommandResult::Account(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "MDBList has no member accounts here",
            )))
        }
    }
}

/// Where a list lives: its owner and slug, or its numeric id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListRef {
    Slug { user: String, slug: String },
    Id(String),
}

fn is_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// Parse `user/slug`, a full list address, or a numeric id.
pub fn parse_list_ref(value: &str) -> Result<ListRef, PluginError> {
    let mut value = value.trim();
    for prefix in ["https://", "http://"] {
        value = value.strip_prefix(prefix).unwrap_or(value);
    }
    value = value.strip_prefix("www.").unwrap_or(value);
    value = value.strip_prefix("mdblist.com/").unwrap_or(value);
    value = value.strip_prefix("lists/").unwrap_or(value);
    let value = value.split(['?', '#']).next().unwrap_or_default();
    let value = value.trim_end_matches('/');
    let value = value.strip_suffix("/json").unwrap_or(value);
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(ListRef::Id(value.to_string()));
    }
    match value.split_once('/') {
        Some((user, slug)) if is_segment(user) && is_segment(slug) => Ok(ListRef::Slug {
            user: user.to_string(),
            slug: slug.to_string(),
        }),
        _ => Err(invalid_config(
            "an MDBList list is its address, user/slug, or numeric id",
        )),
    }
}

fn what(list: &ListRef) -> String {
    match list {
        ListRef::Slug { user, slug } => format!("MDBList list {user}/{slug}"),
        ListRef::Id(id) => format!("MDBList list {id}"),
    }
}

fn site_url(list: &ListRef) -> Option<String> {
    match list {
        ListRef::Slug { user, slug } => Some(format!("{SITE_BASE}/lists/{user}/{slug}")),
        ListRef::Id(_) => None,
    }
}

pub async fn fetch<H: ListHttp>(
    http: &H,
    api_key: Option<&str>,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    if request.source_type != SOURCE_LIST {
        return Err(unsupported_source(&request.source_type));
    }
    let list = parse_list_ref(
        request
            .params
            .get(PARAM_LIST)
            .ok_or_else(|| missing_param(PARAM_LIST))?,
    )?;
    match api_key {
        Some(key) => fetch_api(http, key, &list, request).await,
        None => fetch_public(http, &list, request).await,
    }
}

async fn fetch_public<H: ListHttp>(
    http: &H,
    list: &ListRef,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    let ListRef::Slug { user, slug } = list else {
        return Err(invalid_config(
            "a numeric MDBList id needs the MDBList API key; use the list address instead",
        ));
    };
    let url = format!(
        "{SITE_BASE}/lists/{}/{}/json",
        encode_component(user),
        encode_component(slug)
    );
    let response = http.send(get(url, USER_AGENT, ACCEPT)).await?;
    check_status(&response, Access::Public, &what(list))?;
    let body = json_body(&response)?;
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        return Err(list_provider_common::error::not_found(format!(
            "{} ({error})",
            what(list)
        )));
    }
    Ok(single_page(
        entries(&body),
        None,
        site_url(list),
        request.since_fingerprint.as_deref(),
    ))
}

async fn fetch_api<H: ListHttp>(
    http: &H,
    key: &str,
    list: &ListRef,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    let offset = numeric_cursor(request.page_cursor.as_deref(), 0)?;
    let path = match list {
        ListRef::Slug { user, slug } => {
            format!(
                "/lists/{}/{}/items",
                encode_component(user),
                encode_component(slug)
            )
        }
        ListRef::Id(id) => format!("/lists/{id}/items"),
    };
    let url = format!(
        "{API_BASE}{path}?limit={PAGE_LIMIT}&offset={offset}&apikey={}",
        encode_component(key)
    );
    let response = http.send(get(url, USER_AGENT, ACCEPT)).await?;
    check_status(&response, Access::ServerKey, &what(list))?;
    let body = json_body(&response)?;
    let raw_count = count_entries(&body);
    let mut items = entries(&body);
    let has_more = header(&response, "x-has-more")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
        && raw_count > 0;
    let next_offset = offset.saturating_add(raw_count as u32);
    let within_cap = next_offset < PAGE_LIMIT * MAX_PAGES;

    if offset == 0 && !has_more {
        return Ok(single_page(
            items,
            None,
            site_url(list),
            request.since_fingerprint.as_deref(),
        ));
    }
    // Ranks continue across pages when MDBList gives none of its own.
    for item in &mut items {
        if let Some(rank) = item.rank.as_mut() {
            *rank += offset;
        }
    }
    Ok(ListPluginFetchResponse {
        items,
        next_cursor: (has_more && within_cap).then(|| next_offset.to_string()),
        list_name: None,
        list_url: site_url(list),
        total_hint: header(&response, "x-total-items").and_then(|value| value.trim().parse().ok()),
        fingerprint: None,
        unchanged: false,
    })
}

async fn health<H: ListHttp>(
    http: &H,
    api_key: Option<&str>,
) -> Result<ListPluginHealthResponse, PluginError> {
    let Some(key) = api_key else {
        return Ok(ListPluginHealthResponse {
            healthy: true,
            message: Some("no API key set; only public lists by address are available".to_string()),
        });
    };
    let url = format!("{API_BASE}/user?apikey={}", encode_component(key));
    let response = http.send(get(url, USER_AGENT, ACCEPT)).await?;
    if matches!(response.status, 401 | 403) {
        return Ok(ListPluginHealthResponse {
            healthy: false,
            message: Some("MDBList rejected the API key".to_string()),
        });
    }
    check_status(&response, Access::ServerKey, "MDBList account")?;
    let body = json_body(&response)?;
    let used = body.get("api_requests_count").and_then(Value::as_u64);
    let limit = body.get("api_requests").and_then(Value::as_u64);
    Ok(ListPluginHealthResponse {
        healthy: true,
        message: match (used, limit) {
            (Some(used), Some(limit)) => Some(format!("{used} of {limit} daily API requests used")),
            _ => None,
        },
    })
}

fn raw_entries(body: &Value) -> Vec<&Value> {
    match body {
        Value::Array(entries) => entries.iter().collect(),
        Value::Object(object) => ["movies", "shows", "items"]
            .iter()
            .filter_map(|key| object.get(*key).and_then(Value::as_array))
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
}

fn count_entries(body: &Value) -> usize {
    raw_entries(body).len()
}

/// Items in MDBList's rank order (falling back to response order), movies
/// before shows on equal rank.
fn entries(body: &Value) -> Vec<ListPluginItem> {
    let mut ranked: Vec<(u64, usize, ListPluginItem)> = raw_entries(body)
        .into_iter()
        .enumerate()
        .filter_map(|(position, entry)| {
            let rank = entry
                .get("rank")
                .and_then(Value::as_u64)
                .filter(|rank| *rank > 0)
                .unwrap_or(position as u64 + 1);
            to_item(entry).map(|item| (rank, position, item))
        })
        .collect();
    ranked.sort_by_key(|(rank, position, _)| (*rank, *position));
    let items = ranked.into_iter().map(|(_, _, item)| item).collect();
    dedupe_and_rank(items, 1)
}

fn to_item(entry: &Value) -> Option<ListPluginItem> {
    let kind = match entry.get("mediatype").and_then(Value::as_str) {
        Some("movie") => Some(ListMediaKind::Movie),
        Some("show" | "tv" | "series") => Some(ListMediaKind::Series),
        _ => None,
    };
    let nested = entry.get("ids");
    let ids = Ids::default()
        .with_tmdb(json_id(nested.and_then(|ids| ids.get("tmdb"))))
        .with_imdb(json_text(nested.and_then(|ids| ids.get("imdb"))))
        .with_tvdb(json_id(nested.and_then(|ids| ids.get("tvdb"))))
        .with_imdb(json_text(entry.get("imdb_id")))
        .with_tvdb(json_id(entry.get("tvdb_id")))
        .with_tmdb(json_id(entry.get("tmdb_id")))
        // MDBList's top-level `id` is the TMDb id of the entry.
        .with_tmdb(json_id(entry.get("id")));
    let mut item = build_item(
        &ids,
        kind,
        json_text(entry.get("title")),
        json_year(entry.get("release_year").or_else(|| entry.get("year"))),
    )?;
    item.language = json_text(entry.get("language"));
    Some(item)
}

#[cfg(test)]
mod tests;
