//! Custom list provider: operator-supplied feeds.
//!
//! Three source types, all anonymous:
//!
//! - `rss`: an RSS or Atom feed. Ids are read from `imdb://`, `tmdb://` and
//!   `tvdb://` guids, IMDb, TMDb and TVDB addresses, and id-named elements;
//!   entries with no id fall back to title and year.
//! - `json`: a JSON array, or an object holding one under `items`, `movies`,
//!   `shows`, `results` or `data`. Each entry may carry IMDb, TMDb or TVDB ids
//!   in the spellings the other arrs accept (`imdb_id`, `imdbId`, `tmdbId`,
//!   `tvdbId`, a bare numeric `id` as TMDb, or a nested `ids` object), plus a
//!   title and year.
//! - `stevenlu`: the StevenLu popular-movies JSON, or one of its variants.
//!
//! Ids always win over titles. A title-only entry reaches the host as an
//! unresolved hint.

use list_provider_common::error::{
    Access, check_status, invalid_config, missing_param, plugin_error, unsupported_source,
};
use list_provider_common::feed::parse_feed;
use list_provider_common::http::{HostHttp, ListHttp, body_text, get, host_of, json_body};
use list_provider_common::ids::{
    Ids, build_item, dedupe_and_rank, json_id, json_text, json_year, normalize_imdb,
    split_title_year,
};
use list_provider_common::page::single_page;
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListAuthBadge, ListMediaKind, ListNoteTone, ListPluginFetchRequest, ListPluginFetchResponse,
    ListPluginItem, ListProviderAuth, ListProviderCapabilities, ListProviderDescriptor,
    ListProviderGroup, ListProviderItem, ListProviderNote, ListProviderTile, ListSourceParam,
    ListSourceParamType, ListUrlPattern, ListUrlPatternCapture, PluginDescriptor, PluginError,
    PluginErrorCode, PluginResult, ProviderDescriptor,
};
use serde_json::Value;

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["wit/host-v1.0.0", "wit/runtime-v1.0.0", "wit/list-v1.0.0"],
    generate_all,
});

list_provider_common::list_component_main!(descriptor = descriptor, handler = handle_command,);

pub const PLUGIN_ID: &str = "custom-list";
pub const PROVIDER_TYPE: &str = "custom";
pub const SOURCE_RSS: &str = "rss";
pub const SOURCE_JSON: &str = "json";
pub const SOURCE_STEVENLU: &str = "stevenlu";
pub const PARAM_URL: &str = "url";
pub const STEVENLU_HOST: &str = "popular-movies-data.stevenlu.com";
pub const STEVENLU_URL: &str = "https://popular-movies-data.stevenlu.com/movies.json";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT_FEED: &str =
    "application/rss+xml, application/atom+xml, application/xml;q=0.9, */*;q=0.1";
const ACCEPT_JSON: &str = "application/json";
/// Six hours, the interval the other arrs use for custom feeds.
const DEFAULT_INTERVAL_SECONDS: u64 = 6 * 60 * 60;

fn url_param(label: &str, required: bool) -> ListSourceParam {
    ListSourceParam {
        key: PARAM_URL.to_string(),
        label: label.to_string(),
        param_type: ListSourceParamType::Url,
        options: Vec::new(),
        required,
    }
}

fn source(
    id: &str,
    name: &str,
    description: &str,
    kinds: Vec<ListMediaKind>,
    source_type: &str,
    param: ListSourceParam,
) -> ListProviderItem {
    ListProviderItem {
        id: id.to_string(),
        name: name.to_string(),
        description: Some(description.to_string()),
        kinds,
        source_type: source_type.to_string(),
        params: vec![param],
        personal: false,
        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
    }
}

pub fn descriptor() -> PluginDescriptor {
    let both = || vec![ListMediaKind::Movie, ListMediaKind::Series];
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "Custom".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
        sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
        socket_permissions: Vec::new(),
        provider: ProviderDescriptor::ListProvider(ListProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec!["rss".to_string(), "stevenlu".to_string()],
            summary: Some("RSS, JSON and StevenLu feeds".to_string()),
            blurb: Some(
                "Follow any RSS or JSON feed of movies or shows. Feeds with IMDb, TMDb or \
                 TVDB ids match reliably; titles and years are only a fallback."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#374151".to_string(),
                ink: "#f9fafb".to_string(),
                abbr: "RSS".to_string(),
            }),
            brand_url_template: Some(format!("{{{PARAM_URL}}}")),
            coverage: both(),
            auth: ListProviderAuth::None,
            groups: vec![ListProviderGroup {
                label: "Feeds".to_string(),
                auth_badge: ListAuthBadge::NoAccountNeedsValue,
                items: vec![
                    source("rss", "RSS feed", "An RSS or Atom feed of titles", both(), SOURCE_RSS, url_param("Feed URL", true)),
                    source("json", "JSON feed", "A JSON list with ids or titles", both(), SOURCE_JSON, url_param("Feed URL", true)),
                    source(
                        "stevenlu",
                        "StevenLu popular movies",
                        "Popular movies from the StevenLu feed",
                        vec![ListMediaKind::Movie],
                        SOURCE_STEVENLU,
                        url_param("Feed URL (optional)", false),
                    ),
                ],
            }],
            notes: vec![ListProviderNote {
                tone: ListNoteTone::Info,
                text_key: "lists.note.custom_prefer_ids".to_string(),
            }],
            url_patterns: vec![ListUrlPattern {
                pattern: r"^(?<url>https?://popular-movies-data\.stevenlu\.com/movies(?:-[0-9A-Za-z-]+)?\.json)$"
                    .to_string(),
                source_type: SOURCE_STEVENLU.to_string(),
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
            allowed_hosts: vec![STEVENLU_HOST.to_string()],
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
            PluginListCommandResult::Fetch(match fetch(http, &request).await {
                Ok(response) => PluginResult::Ok(response),
                Err(error) => PluginResult::Err(error),
            })
        }
        PluginListCommand::Account(_) => {
            PluginListCommandResult::Account(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "custom feeds have no accounts",
            )))
        }
        PluginListCommand::Health(_) => {
            PluginListCommandResult::Health(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "custom feeds have no server key to check",
            )))
        }
    }
}

/// An absolute http(s) URL with a host, or an invalid-config error.
fn feed_url(
    request: &ListPluginFetchRequest,
    default: Option<&str>,
) -> Result<String, PluginError> {
    let url = request
        .params
        .get(PARAM_URL)
        .map(|url| url.trim())
        .filter(|url| !url.is_empty())
        .or(default)
        .ok_or_else(|| missing_param(PARAM_URL))?;
    if host_of(url).is_none() || url.contains(char::is_whitespace) {
        return Err(invalid_config(
            "the feed URL must be an absolute http or https address",
        ));
    }
    Ok(url.to_string())
}

pub async fn fetch<H: ListHttp>(
    http: &H,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    let (url, accept) = match request.source_type.as_str() {
        SOURCE_RSS => (feed_url(request, None)?, ACCEPT_FEED),
        SOURCE_JSON => (feed_url(request, None)?, ACCEPT_JSON),
        SOURCE_STEVENLU => (feed_url(request, Some(STEVENLU_URL))?, ACCEPT_JSON),
        other => return Err(unsupported_source(other)),
    };
    let response = http.send(get(url.clone(), USER_AGENT, accept)).await?;
    check_status(&response, Access::Public, "feed")?;

    let (items, list_name, list_url) = match request.source_type.as_str() {
        SOURCE_RSS => {
            let feed = parse_feed(&body_text(&response))?;
            let items = feed
                .entries
                .into_iter()
                .filter_map(|entry| build_item(&entry.ids, entry.kind, entry.title, entry.year))
                .collect();
            (items, feed.title, Some(url))
        }
        SOURCE_JSON => (json_items(&json_body(&response)?, None), None, Some(url)),
        _ => (
            json_items(&json_body(&response)?, Some(ListMediaKind::Movie)),
            Some("StevenLu popular movies".to_string()),
            Some(url),
        ),
    };
    Ok(single_page(
        dedupe_and_rank(items, 1),
        list_name,
        list_url,
        request.since_fingerprint.as_deref(),
    ))
}

/// Look up a field by any of `names`, ignoring case and `_`/`-`.
fn field<'a>(entry: &'a Value, names: &[&str]) -> Option<&'a Value> {
    let object = entry.as_object()?;
    names.iter().find_map(|name| {
        object
            .iter()
            .find(|(key, value)| {
                !value.is_null()
                    && key
                        .chars()
                        .filter(|ch| !matches!(ch, '_' | '-'))
                        .map(|ch| ch.to_ascii_lowercase())
                        .eq(name.chars())
            })
            .map(|(_, value)| value)
    })
}

fn kind_of(entry: &Value) -> Option<ListMediaKind> {
    let text = json_text(field(entry, &["mediatype", "type", "kind"]))?.to_ascii_lowercase();
    match text.as_str() {
        "movie" | "movies" | "film" => Some(ListMediaKind::Movie),
        "show" | "shows" | "tv" | "series" | "tvshow" => Some(ListMediaKind::Series),
        _ => None,
    }
}

fn entry_item(entry: &Value, kind: Option<ListMediaKind>) -> Option<ListPluginItem> {
    let kind = kind_of(entry).or(kind);
    let nested = field(entry, &["ids"]);
    let from_nested = |names: &[&str]| nested.and_then(|ids| field(ids, names));
    let bare_id = field(entry, &["id"]);
    let ids = Ids::default()
        .with_imdb(json_text(field(entry, &["imdbid", "imdb"])))
        .with_tmdb(json_id(field(entry, &["tmdbid", "tmdb"])))
        .with_tvdb(json_id(field(entry, &["tvdbid", "tvdb"])))
        .with_imdb(json_text(from_nested(&["imdb", "imdbid"])))
        .with_tmdb(json_id(from_nested(&["tmdb", "tmdbid"])))
        .with_tvdb(json_id(from_nested(&["tvdb", "tvdbid"])))
        // A bare `id` is TMDb's, as Radarr's custom lists define it, unless it
        // is an IMDb id.
        .with_imdb(json_text(bare_id).and_then(|id| normalize_imdb(&id)))
        .with_tmdb(json_id(bare_id));
    let raw_title = json_text(field(entry, &["title", "name", "movietitle"]));
    let (title, title_year) = match raw_title {
        Some(raw) => {
            let (title, year) = split_title_year(&raw);
            (Some(title), year)
        }
        None => (None, None),
    };
    let year = json_year(field(entry, &["year", "releaseyear"])).or(title_year);
    build_item(&ids, kind, title, year)
}

/// Entries of a JSON feed, each with the kind its container implies.
fn json_items(body: &Value, default_kind: Option<ListMediaKind>) -> Vec<ListPluginItem> {
    let mut groups: Vec<(Option<ListMediaKind>, &Vec<Value>)> = Vec::new();
    match body {
        Value::Array(entries) => groups.push((default_kind, entries)),
        Value::Object(_) => {
            for (names, kind) in [
                (&["movies"][..], Some(ListMediaKind::Movie)),
                (&["shows", "series"][..], Some(ListMediaKind::Series)),
                (&["items", "results", "data", "list"][..], default_kind),
            ] {
                if let Some(entries) = field(body, names).and_then(Value::as_array) {
                    groups.push((kind, entries));
                }
            }
        }
        _ => {}
    }
    groups
        .into_iter()
        .flat_map(|(kind, entries)| {
            entries
                .iter()
                .filter_map(move |entry| entry_item(entry, kind))
        })
        .collect()
}

#[cfg(test)]
mod tests;
