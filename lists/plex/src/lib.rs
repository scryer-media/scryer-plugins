//! Plex watchlist list provider.
//!
//! Plex publishes a member's watchlist as an RSS feed at
//! `https://rss.plex.tv/<feed id>` once the member turns the feed on in the
//! Plex web app. The feed is public to anyone holding the URL, so this plugin
//! needs no account and stores nothing: the operator pastes the URL, the host
//! recognises it through the descriptor's URL pattern, and every sync reads
//! the feed once. Each entry carries `imdb://`, `tmdb://` and `tvdb://` guids
//! and a `movie` or `show` category, which is all the host needs to resolve
//! it. Entries without any id are skipped rather than guessed from a title.
//!
//! The signed-in watchlist (and friends' watchlists) needs a Plex account
//! token and belongs to the member-account flow, not this plugin.

use list_provider_common::error::{
    Access, check_status, missing_param, plugin_error, unsupported_source,
};
use list_provider_common::feed::parse_feed;
use list_provider_common::http::{HostHttp, ListHttp, body_text, get};
use list_provider_common::ids::{build_item, dedupe_and_rank};
use list_provider_common::page::single_page;
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListAuthBadge, ListMediaKind, ListNoteTone, ListPluginFetchRequest, ListPluginFetchResponse,
    ListPluginItem, ListProviderAuth, ListProviderCapabilities, ListProviderDescriptor,
    ListProviderGroup, ListProviderItem, ListProviderNote, ListProviderTile, ListSourceParam,
    ListSourceParamType, ListUrlPattern, ListUrlPatternCapture, PluginDescriptor, PluginError,
    PluginErrorCode, PluginResult, ProviderDescriptor,
};

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["wit/host-v1.0.0", "wit/runtime-v1.0.0", "wit/list-v1.0.0"],
    generate_all,
});

list_provider_common::list_component_main!(descriptor = descriptor, handler = handle_command,);

pub const PLUGIN_ID: &str = "plex-list";
pub const PROVIDER_TYPE: &str = "plex";
pub const SOURCE_WATCHLIST_RSS: &str = "watchlist_rss";
pub const PARAM_URL: &str = "url";
const FEED_HOST: &str = "rss.plex.tv";
const FEED_PREFIX: &str = "https://rss.plex.tv/";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/rss+xml, application/xml;q=0.9, */*;q=0.1";
/// Six hours, the watchlist interval the other arrs use.
const DEFAULT_INTERVAL_SECONDS: u64 = 6 * 60 * 60;

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
            summary: Some("A Plex watchlist through its RSS feed".to_string()),
            blurb: Some(
                "Turn on the watchlist RSS feed in Plex and paste its rss.plex.tv address. \
                 Movies and shows are matched by the ids Plex puts in the feed."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#1f1f1f".to_string(),
                ink: "#e5a00d".to_string(),
                abbr: "PX".to_string(),
            }),
            brand_url_template: Some("{url}".to_string()),
            coverage: vec![ListMediaKind::Movie, ListMediaKind::Series],
            auth: ListProviderAuth::None,
            groups: vec![ListProviderGroup {
                label: "Watchlist".to_string(),
                auth_badge: ListAuthBadge::NoAccountNeedsValue,
                items: vec![ListProviderItem {
                    id: "watchlist-rss".to_string(),
                    name: "Watchlist RSS feed".to_string(),
                    description: Some(
                        "Any watchlist whose RSS feed is turned on, by its rss.plex.tv address"
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
            }],
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
                account: false,
                health: false,
                requires_member_credential: false,
            },
            config_fields: Vec::new(),
            default_base_url: None,
            allowed_hosts: vec![FEED_HOST.to_string()],
            rate_limit_seconds: None,
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
        PluginListCommand::Account(_) => {
            PluginListCommandResult::Account(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "Plex accounts are not supported by the RSS feed",
            )))
        }
        PluginListCommand::Health(_) => {
            PluginListCommandResult::Health(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "the Plex RSS feed has no server key to check",
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
    if request.source_type != SOURCE_WATCHLIST_RSS {
        return Err(unsupported_source(&request.source_type));
    }
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

#[cfg(test)]
mod tests;
