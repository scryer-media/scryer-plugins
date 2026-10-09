//! Plex watchlist list provider.
//!
//! Two sources read a Plex watchlist.
//!
//! `watchlist_rss` reads the RSS feed Plex publishes at
//! `https://rss.plex.tv/<feed id>` once the member turns the feed on in the
//! Plex web app. The feed is public to anyone holding the URL, so it needs no
//! account: the operator pastes the URL, the host recognises it through the
//! descriptor's URL pattern, and every sync reads the feed once.
//!
//! `watchlist` is personal. It reads the linked member's own watchlist from
//! Plex's discover service with the account token the host holds for that
//! member, sent only in the `X-Plex-Token` header and never on a URL. The
//! service pages by container offset, so the source fails past [`MAX_PAGES`]
//! rather than cutting the list short, and leaves the change fingerprint
//! unset. Each title's first
//! release date sets `released`, which the host's released-only filter reads.
//!
//! Both sources read `imdb://`, `tmdb://` and `tvdb://` guids and a movie or
//! show type, so the same title gets the same item key from either one.
//! Entries without any id are skipped rather than guessed from a title.
//!
//! Friends' watchlists are not offered: Plex serves them only through an
//! undocumented GraphQL service that returns titles without ids.

use list_provider_common::error::{
    Access, auth_failed, check_status, missing_param, permanent, plugin_error, unavailable,
    unsupported_source,
};
use list_provider_common::feed::parse_feed;
use list_provider_common::http::{
    HostHttp, ListHttp, body_text, encode_component, get, header, json_body,
};
use list_provider_common::ids::{
    Ids, build_item, dedupe_and_rank, json_text, json_year, year_from_date,
};
use list_provider_common::page::single_page;
use scryer_plugin_pdk::runtime;
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{
    ListAccountExchange, ListAccountFlow, ListAuthBadge, ListCredential, ListMediaKind,
    ListNoteTone, ListPluginAccountRequest, ListPluginAccountResponse, ListPluginFetchRequest,
    ListPluginFetchResponse, ListPluginItem, ListProviderAuth, ListProviderCapabilities,
    ListProviderDescriptor, ListProviderGroup, ListProviderItem, ListProviderNote,
    ListProviderTile, ListSourceParam, ListSourceParamType, ListUrlPattern, ListUrlPatternCapture,
    PluginDescriptor, PluginError, PluginErrorCode, PluginResult, ProviderDescriptor,
};
use serde_json::Value;

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/runtime-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/list-v1.0.0"],
    generate_all,
});

list_provider_common::list_component_main!(descriptor = descriptor, handler = handle_command,);

pub const PLUGIN_ID: &str = "plex-list";
pub const PROVIDER_TYPE: &str = "plex";
pub const SOURCE_WATCHLIST_RSS: &str = "watchlist_rss";
/// The linked member's own watchlist.
pub const SOURCE_WATCHLIST: &str = "watchlist";
pub const PARAM_URL: &str = "url";
const FEED_HOST: &str = "rss.plex.tv";
const FEED_PREFIX: &str = "https://rss.plex.tv/";
const DISCOVER_HOST: &str = "discover.provider.plex.tv";
const ACCOUNT_HOST: &str = "plex.tv";
pub const WATCHLIST_URL: &str = "https://discover.provider.plex.tv/library/sections/watchlist/all";
pub const ACCOUNT_URL: &str = "https://plex.tv/users/account.json";
const TOKEN_HEADER: &str = "X-Plex-Token";
/// The client identity Plex asks every client to send, the same one Scryer's
/// Plex notification plugin uses. Plex documents the client identifier as
/// typically required; the rest describe the client.
const PLEX_CLIENT_IDENTIFIER: &str = "scryer";
const PLEX_PRODUCT: &str = "Scryer";
const PLEX_PLATFORM: &str = "Scryer";
const PLEX_DEVICE_NAME: &str = "Scryer";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/rss+xml, application/xml;q=0.9, */*;q=0.1";
const ACCEPT_JSON: &str = "application/json";
/// Six hours, the watchlist interval the other arrs use.
const DEFAULT_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
/// Spacing between Plex requests, the one the other arrs keep for Plex.
const RATE_LIMIT_SECONDS: i64 = 5;
/// Titles asked for per watchlist page, the page size the other arrs use.
pub const PAGE_SIZE: u32 = 100;
/// Requests per sync of the member watchlist: two thousand titles at full
/// pages, and well inside the host's hundred-page ceiling.
pub const MAX_PAGES: u32 = 20;
const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "Plex".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
        sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
        socket_permissions: Vec::new(),
        provider: ProviderDescriptor::ListProvider(ListProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: Vec::new(),
            summary: Some(
                "A Plex watchlist through its RSS feed or a linked Plex account".to_string(),
            ),
            blurb: Some(
                "Paste the rss.plex.tv address of any watchlist whose RSS feed is turned on, \
                 or link your Plex account to follow your own watchlist. Movies and shows are \
                 matched by the ids Plex gives them."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#1f1f1f".to_string(),
                ink: "#e5a00d".to_string(),
                abbr: "PX".to_string(),
            }),
            brand_url_template: Some("{url}".to_string()),
            coverage: vec![ListMediaKind::Movie, ListMediaKind::Series],
            // The account serves the personal group only. The RSS source is
            // not personal and works without one.
            auth: ListProviderAuth::MemberAccount {
                flow: ListAccountFlow::PlexPin,
                exchange: ListAccountExchange::Direct,
                byo_app: false,
                scopes: Vec::new(),
            },
            groups: vec![
                ListProviderGroup {
                    label: "Watchlist".to_string(),
                    auth_badge: ListAuthBadge::NoAccountNeedsValue,
                    items: vec![ListProviderItem {
                        id: "watchlist-rss".to_string(),
                        name: "Watchlist RSS feed".to_string(),
                        description: Some(
                            "Any watchlist whose RSS feed is turned on, by its rss.plex.tv \
                             address"
                                .to_string(),
                        ),
                        kinds: vec![ListMediaKind::Movie, ListMediaKind::Series],
                        source_type: SOURCE_WATCHLIST_RSS.to_string(),
                        params: vec![ListSourceParam {
                            key: PARAM_URL.to_string(),
                            label: "RSS feed URL".to_string(),
                            param_type: ListSourceParamType::Url,
                            options: Vec::new(),
                            required: true,
                        }],
                        personal: false,
                        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
                    }],
                },
                ListProviderGroup {
                    label: "Your Plex account".to_string(),
                    auth_badge: ListAuthBadge::MemberAccount,
                    items: vec![ListProviderItem {
                        id: "watchlist".to_string(),
                        name: "Your watchlist".to_string(),
                        description: Some(
                            "The watchlist of your linked Plex account, with no RSS feed needed"
                                .to_string(),
                        ),
                        kinds: vec![ListMediaKind::Movie, ListMediaKind::Series],
                        source_type: SOURCE_WATCHLIST.to_string(),
                        params: Vec::new(),
                        personal: true,
                        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
                    }],
                },
            ],
            notes: vec![ListProviderNote {
                tone: ListNoteTone::Warn,
                text_key: "lists.note.plex_watchlist_endpoint".to_string(),
            }],
            url_patterns: vec![ListUrlPattern {
                pattern: r"^(?<url>https://rss\.plex\.tv/[0-9A-Za-z-]+)/?$".to_string(),
                source_type: SOURCE_WATCHLIST_RSS.to_string(),
                captures: vec![ListUrlPatternCapture {
                    group: "url".to_string(),
                    param: PARAM_URL.to_string(),
                }],
            }],
            capabilities: ListProviderCapabilities {
                account: true,
                health: false,
                // The RSS source works with no account at all.
                requires_member_credential: false,
            },
            config_fields: Vec::new(),
            default_base_url: None,
            allowed_hosts: vec![
                FEED_HOST.to_string(),
                DISCOVER_HOST.to_string(),
                ACCOUNT_HOST.to_string(),
            ],
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
            PluginListCommandResult::Account(into_result(account(http, &request).await))
        }
        PluginListCommand::Health(_) => {
            PluginListCommandResult::Health(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "Plex has no server key to check",
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

/// Accept only an https rss.plex.tv address, so the parameter can never point
/// the plugin anywhere else.
fn feed_url(request: &ListPluginFetchRequest) -> Result<String, PluginError> {
    let url = request
        .params
        .get(PARAM_URL)
        .map(|url| url.trim())
        .filter(|url| !url.is_empty())
        .ok_or_else(|| missing_param(PARAM_URL))?;
    let path = url.strip_prefix(FEED_PREFIX).unwrap_or_default();
    if path.is_empty() || path.contains(['?', '#', '@']) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "the Plex watchlist feed must be an https://rss.plex.tv/ address",
        ));
    }
    Ok(url.to_string())
}

pub async fn fetch<H: ListHttp>(
    http: &H,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    fetch_at(http, request, runtime::wall_now_ms()).await
}

/// [`fetch`] against a given wall clock, in milliseconds since the Unix
/// epoch. Zero means the clock is unknown, which leaves `released` unset.
pub async fn fetch_at<H: ListHttp>(
    http: &H,
    request: &ListPluginFetchRequest,
    now_ms: u64,
) -> Result<ListPluginFetchResponse, PluginError> {
    match request.source_type.as_str() {
        SOURCE_WATCHLIST_RSS => fetch_feed(http, request).await,
        SOURCE_WATCHLIST => fetch_watchlist(http, request, today(now_ms)).await,
        other => Err(unsupported_source(other)),
    }
}

/// The public feed. It is never sent the member's token, whatever the
/// request carries.
async fn fetch_feed<H: ListHttp>(
    http: &H,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    let url = feed_url(request)?;
    let response = http.send(get(url.clone(), USER_AGENT, ACCEPT)).await?;
    check_status(&response, Access::Public, "Plex watchlist feed")?;
    let feed = parse_feed(&body_text(&response))?;
    let items: Vec<ListPluginItem> = feed
        .entries
        .into_iter()
        .filter(|entry| !entry.ids.is_empty())
        .filter_map(|entry| build_item(&entry.ids, entry.kind, entry.title, entry.year))
        .collect();
    Ok(single_page(
        dedupe_and_rank(items, 1),
        feed.title,
        Some(url),
        request.since_fingerprint.as_deref(),
    ))
}

/// The linked member's token. A personal source without one cannot be read,
/// and a malformed one asks for the account to be linked again; the token
/// itself never appears in a message.
fn member_token(credential: Option<&ListCredential>) -> Result<&str, PluginError> {
    let token = credential
        .map(|credential| credential.access_token.trim())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| auth_failed("this watchlist needs a linked Plex account"))?;
    if token
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(auth_failed("the linked Plex account has an unusable token"));
    }
    Ok(token)
}

/// A GET that carries the member's token in its header, never in the URL,
/// along with the client identity Plex expects.
fn member_get(url: String, token: &str) -> PluginHttpRequest {
    let mut request = get(url, USER_AGENT, ACCEPT_JSON);
    for (name, value) in [
        ("X-Plex-Client-Identifier", PLEX_CLIENT_IDENTIFIER),
        ("X-Plex-Product", PLEX_PRODUCT),
        ("X-Plex-Version", env!("CARGO_PKG_VERSION")),
        ("X-Plex-Platform", PLEX_PLATFORM),
        ("X-Plex-Device-Name", PLEX_DEVICE_NAME),
        (TOKEN_HEADER, token),
    ] {
        request.headers.insert(name.to_string(), value.to_string());
    }
    request
}

/// Plex answers a token it no longer accepts with 401 (or 403), which marks
/// the linked account expired. A 404 means the service moved, not that the
/// member's watchlist is gone, so it stays a plain failure.
fn check_member_status(response: &PluginHttpResponse, what: &str) -> Result<(), PluginError> {
    match response.status {
        401 | 403 => Err(auth_failed(format!(
            "Plex no longer accepts the linked account (HTTP {})",
            response.status
        ))),
        404 | 410 => Err(permanent(format!(
            "Plex no longer serves the {what} at this address (HTTP {})",
            response.status
        ))),
        _ => check_status(response, Access::ServerKey, what),
    }
}

/// The member watchlist's cursor: the request number, then the container
/// offset. Plex may answer with fewer titles than asked, so the offset alone
/// cannot count requests against [`MAX_PAGES`].
fn watchlist_cursor(cursor: Option<&str>) -> Result<(u32, u32), PluginError> {
    let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) else {
        return Ok((0, 0));
    };
    let invalid = || permanent(format!("invalid page cursor {cursor}"));
    let (page, offset) = cursor.split_once(':').ok_or_else(invalid)?;
    let page = page.parse::<u32>().map_err(|_| invalid())?;
    let offset = offset.parse::<u32>().map_err(|_| invalid())?;
    if page >= MAX_PAGES {
        return Err(invalid());
    }
    Ok((page, offset))
}

async fn fetch_watchlist<H: ListHttp>(
    http: &H,
    request: &ListPluginFetchRequest,
    today: Option<i64>,
) -> Result<ListPluginFetchResponse, PluginError> {
    let token = member_token(request.credential.as_ref())?;
    let (page, offset) = watchlist_cursor(request.page_cursor.as_deref())?;
    let url = format!(
        "{WATCHLIST_URL}?includeGuids=1&excludeElements=Image&sort={}\
         &X-Plex-Container-Start={offset}&X-Plex-Container-Size={PAGE_SIZE}",
        encode_component("watchlistedAt:desc"),
    );
    let response = http.send(member_get(url, token)).await?;
    check_member_status(&response, "Plex watchlist")?;
    let body = json_body(&response)?;
    let container = body
        .get("MediaContainer")
        .ok_or_else(|| permanent("the Plex watchlist response has no MediaContainer"))?;
    let entries = container
        .get("Metadata")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    let count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    // Plex may answer with fewer titles than asked, so only the total it
    // reports can say where the watchlist ends. Without one the end cannot be
    // told from a short page, and guessing would hand the host a shortened
    // list whose tail reads as having left it.
    let total = container
        .get("totalSize")
        .and_then(Value::as_u64)
        .or_else(|| {
            header(&response, "X-Plex-Container-Total-Size")
                .and_then(|total| total.trim().parse::<u64>().ok())
        })
        .ok_or_else(|| {
            permanent("the Plex watchlist response does not say how many titles it holds")
        })?;
    let next_offset = offset.saturating_add(count);
    let more = u64::from(next_offset) < total;
    if more && count == 0 {
        return Err(unavailable(format!(
            "Plex returned an empty watchlist page at {offset} of {total} titles"
        )));
    }
    // A watchlist past the cap fails rather than being cut short: the host
    // would read every title after the cap as having left the list.
    let cap = u64::from(MAX_PAGES * PAGE_SIZE);
    if total > cap || (more && page + 1 >= MAX_PAGES) {
        return Err(permanent(format!(
            "the Plex watchlist has more than {cap} titles, more than Scryer follows"
        )));
    }
    let next_cursor = more.then(|| format!("{}:{next_offset}", page + 1));

    let items: Vec<ListPluginItem> = entries
        .iter()
        .filter_map(|entry| watchlist_item(entry, today))
        .collect();
    Ok(ListPluginFetchResponse {
        items: dedupe_and_rank(items, offset.saturating_add(1)),
        next_cursor,
        list_name: Some("Plex watchlist".to_string()),
        list_url: None,
        total_hint: Some(total.min(cap) as u32),
        // A paged source carries no fingerprint: the host compares only the
        // first page. `released` also changes with the date alone.
        fingerprint: None,
        unchanged: false,
    })
}

/// One watchlist title, or `None` when Plex gave it no usable id.
fn watchlist_item(entry: &Value, today: Option<i64>) -> Option<ListPluginItem> {
    let ids = guid_ids(entry);
    if ids.is_empty() {
        return None;
    }
    let kind = match entry
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("movie") => Some(ListMediaKind::Movie),
        Some("show") => Some(ListMediaKind::Series),
        _ => None,
    };
    let date = entry.get("originallyAvailableAt").and_then(Value::as_str);
    let year = json_year(entry.get("year")).or_else(|| date.and_then(year_from_date));
    let mut item = build_item(&ids, kind, json_text(entry.get("title")), year)?;
    item.released = released(date, year, today);
    Some(item)
}

/// The `imdb://`, `tmdb://` and `tvdb://` ids in a title's `Guid` array.
/// Plex's own `plex://` guids are left out: nothing downstream resolves them.
fn guid_ids(entry: &Value) -> Ids {
    let mut ids = Ids::default();
    let guids = entry.get("Guid").and_then(Value::as_array);
    for guid in guids.into_iter().flatten() {
        let Some((scheme, value)) = guid
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.trim().split_once("://"))
        else {
            continue;
        };
        let value = Some(value.to_string());
        ids = match scheme.to_ascii_lowercase().as_str() {
            "tmdb" => ids.with_tmdb(value),
            "imdb" => ids.with_imdb(value),
            "tvdb" => ids.with_tvdb(value),
            _ => ids,
        };
    }
    ids
}

/// Days since the Unix epoch, or `None` when the clock is unknown.
fn today(now_ms: u64) -> Option<i64> {
    (now_ms > 0).then_some((now_ms / MILLIS_PER_DAY) as i64)
}

/// Whether a title has come out by `today`, from its first release date or,
/// without one, its year. Unknown when the clock or both dates are missing,
/// or when only the year is known and it is the current one.
fn released(date: Option<&str>, year: Option<i32>, today: Option<i64>) -> Option<bool> {
    let today = today?;
    if let Some(day) = date.and_then(epoch_day) {
        return Some(day <= today);
    }
    let year = i64::from(year?);
    if days_from_civil(year + 1, 1, 1) <= today {
        Some(true)
    } else if days_from_civil(year, 1, 1) > today {
        Some(false)
    } else {
        None
    }
}

/// Days since the Unix epoch of a `YYYY-MM-DD` date.
fn epoch_day(date: &str) -> Option<i64> {
    let mut parts = date.trim().get(..10)?.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The linked member's plex.tv identity. The token goes in the header only;
/// the email address Plex also returns is never read.
pub async fn account<H: ListHttp>(
    http: &H,
    request: &ListPluginAccountRequest,
) -> Result<ListPluginAccountResponse, PluginError> {
    let token = member_token(Some(&request.credential))?;
    let response = http
        .send(member_get(ACCOUNT_URL.to_string(), token))
        .await?;
    check_member_status(&response, "Plex account")?;
    let body = json_body(&response)?;
    let user = body
        .get("user")
        .filter(|user| user.is_object())
        .ok_or_else(|| permanent("the Plex account response has no user"))?;
    let external_user_id = json_text(user.get("id"))
        .or_else(|| json_text(user.get("uuid")))
        .ok_or_else(|| permanent("the Plex account response has no user id"))?;
    let display_name = json_text(user.get("title"));
    let username = json_text(user.get("username"))
        .or_else(|| display_name.clone())
        .unwrap_or_else(|| external_user_id.clone());
    Ok(ListPluginAccountResponse {
        external_user_id,
        username,
        display_name,
        avatar_url: json_text(user.get("thumb")).filter(|url| url.starts_with("https://")),
        owned_lists: Vec::new(),
        statuses: Vec::new(),
    })
}

#[cfg(test)]
mod tests;
