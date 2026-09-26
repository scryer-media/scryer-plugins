//! TMDb list provider.
//!
//! Follows four kinds of public TMDb sources with the server's TMDb API key:
//!
//! - `list`: a public list by id, paged through `/3/list/{id}`.
//! - `person`: everything a person is credited on, from one
//!   `/3/person/{id}?append_to_response=combined_credits` read.
//! - `company` and `keyword`: TMDb discover filtered by the company or
//!   keyword, newest first, paged.
//!
//! Parameterless charts (popular, top rated, upcoming and the rest) are served
//! by the metadata gateway and are deliberately absent here.
//!
//! The key is either a v3 API key (sent as `api_key`) or a v4 read access
//! token (sent as a bearer token). It is server configuration declared in the
//! descriptor; no member credential is ever read or stored.

use list_provider_common::error::{
    Access, auth_failed, check_status, invalid_config, missing_param, not_found, plugin_error,
    unsupported_source,
};
use list_provider_common::http::{HostHttp, ListHttp, encode_component, get, json_body};
use list_provider_common::ids::{
    Ids, build_item, dedupe_and_rank, json_id, json_text, year_from_date,
};
use list_provider_common::page::{numeric_cursor, single_page};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ListAuthBadge, ListMediaKind, ListNoteTone,
    ListPluginFetchRequest, ListPluginFetchResponse, ListPluginHealthResponse, ListPluginItem,
    ListProviderAuth, ListProviderCapabilities, ListProviderDescriptor, ListProviderGroup,
    ListProviderItem, ListProviderNote, ListProviderRating, ListProviderTile, ListSourceParam,
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

pub const PLUGIN_ID: &str = "tmdb-list";
pub const PROVIDER_TYPE: &str = "tmdb";
pub const CONFIG_API_KEY: &str = "api_key";

pub const SOURCE_LIST: &str = "list";
pub const SOURCE_PERSON: &str = "person";
pub const SOURCE_COMPANY: &str = "company";
pub const SOURCE_KEYWORD: &str = "keyword";

pub const PARAM_LIST_ID: &str = "list_id";
pub const PARAM_PERSON_ID: &str = "person_id";
pub const PARAM_COMPANY_ID: &str = "company_id";
pub const PARAM_KEYWORD_ID: &str = "keyword_id";
pub const PARAM_CREDIT: &str = "credit";
pub const PARAM_KIND: &str = "kind";

pub const API_BASE: &str = "https://api.themoviedb.org/3";
const API_HOST: &str = "api.themoviedb.org";
const SITE_BASE: &str = "https://www.themoviedb.org";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/json";
/// TMDb's fixed page size for lists and discover.
const PAGE_SIZE: u32 = 20;
/// Deepest discover page followed: 1,000 titles, well inside the host's
/// hundred-page ceiling per sync and TMDb's own 500-page discover limit.
pub const MAX_PAGES: u32 = 50;
/// Twelve hours, the interval the other arrs use for TMDb lists.
const DEFAULT_INTERVAL_SECONDS: u64 = 12 * 60 * 60;
/// TMDb status codes that mean the key itself is bad, as opposed to a
/// resource the key may not read.
const TMDB_INVALID_KEY_CODES: [i64; 3] = [7, 10, 30];
/// TV genres whose credits are appearances rather than work: talk shows and
/// news.
const APPEARANCE_TV_GENRES: [i64; 2] = [10767, 10763];

fn text_param(key: &str, label: &str) -> ListSourceParam {
    ListSourceParam {
        key: key.to_string(),
        label: label.to_string(),
        param_type: ListSourceParamType::Text,
        options: Vec::new(),
        required: true,
    }
}

fn enum_param(key: &str, label: &str, options: &[&str]) -> ListSourceParam {
    ListSourceParam {
        key: key.to_string(),
        label: label.to_string(),
        param_type: ListSourceParamType::Enum,
        options: options.iter().map(|option| option.to_string()).collect(),
        required: false,
    }
}

fn source_item(
    id: &str,
    name: &str,
    description: &str,
    kinds: Vec<ListMediaKind>,
    source_type: &str,
    params: Vec<ListSourceParam>,
) -> ListProviderItem {
    ListProviderItem {
        id: id.to_string(),
        name: name.to_string(),
        description: Some(description.to_string()),
        kinds,
        source_type: source_type.to_string(),
        params,
        personal: false,
        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
    }
}

fn url_pattern(path: &str, group: &str, source_type: &str) -> ListUrlPattern {
    ListUrlPattern {
        pattern: format!(
            r"^https?://(?:www\.)?themoviedb\.org/{path}/(?<{group}>\d+)(?:[-/?#].*)?$"
        ),
        source_type: source_type.to_string(),
        captures: vec![ListUrlPatternCapture {
            group: group.to_string(),
            param: group.to_string(),
        }],
    }
}

pub fn descriptor() -> PluginDescriptor {
    let both = || vec![ListMediaKind::Movie, ListMediaKind::Series];
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "TMDb".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
        sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
        socket_permissions: Vec::new(),
        provider: ProviderDescriptor::ListProvider(ListProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec!["themoviedb".to_string()],
            summary: Some("Public TMDb lists, people, companies and keywords".to_string()),
            blurb: Some(
                "Follow a public TMDb list, everything a person is credited on, or \
                 everything from a company or keyword. Charts such as popular and top \
                 rated come from Scryer's metadata service instead."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#0d253f".to_string(),
                ink: "#01b4e4".to_string(),
                abbr: "TM".to_string(),
            }),
            brand_url_template: None,
            coverage: both(),
            auth: ListProviderAuth::ServerApiKey {
                config_field: CONFIG_API_KEY.to_string(),
            },
            groups: vec![ListProviderGroup {
                label: "Public sources".to_string(),
                auth_badge: ListAuthBadge::ServerApiKey,
                items: vec![
                    source_item(
                        "public-list",
                        "Public list",
                        "A public TMDb list, by its id or address",
                        both(),
                        SOURCE_LIST,
                        vec![text_param(PARAM_LIST_ID, "List id")],
                    ),
                    source_item(
                        "person",
                        "Person",
                        "Movies and shows a person is credited on",
                        both(),
                        SOURCE_PERSON,
                        vec![
                            text_param(PARAM_PERSON_ID, "Person id"),
                            enum_param(PARAM_CREDIT, "Credits", &["cast", "crew", "all"]),
                            enum_param(PARAM_KIND, "Media", &["all", "movie", "series"]),
                        ],
                    ),
                    source_item(
                        "company",
                        "Company",
                        "Titles from a production company, newest first",
                        both(),
                        SOURCE_COMPANY,
                        vec![
                            text_param(PARAM_COMPANY_ID, "Company id"),
                            enum_param(PARAM_KIND, "Media", &["movie", "series"]),
                        ],
                    ),
                    source_item(
                        "keyword",
                        "Keyword",
                        "Titles tagged with a keyword, newest first",
                        both(),
                        SOURCE_KEYWORD,
                        vec![
                            text_param(PARAM_KEYWORD_ID, "Keyword id"),
                            enum_param(PARAM_KIND, "Media", &["movie", "series"]),
                        ],
                    ),
                ],
            }],
            notes: vec![ListProviderNote {
                tone: ListNoteTone::Info,
                text_key: "lists.note.tmdb_commercial".to_string(),
            }],
            url_patterns: vec![
                url_pattern("list", PARAM_LIST_ID, SOURCE_LIST),
                url_pattern("person", PARAM_PERSON_ID, SOURCE_PERSON),
                url_pattern("company", PARAM_COMPANY_ID, SOURCE_COMPANY),
                url_pattern("keyword", PARAM_KEYWORD_ID, SOURCE_KEYWORD),
            ],
            capabilities: ListProviderCapabilities {
                account: false,
                health: true,
                requires_member_credential: false,
            },
            config_fields: vec![ConfigFieldDef {
                key: CONFIG_API_KEY.to_string(),
                label: "TMDb API key or read access token".to_string(),
                field_type: ConfigFieldType::Password,
                required: true,
                help_text: Some(
                    "A v3 API key or a v4 API read access token from your TMDb account settings."
                        .to_string(),
                ),
                ..Default::default()
            }],
            default_base_url: None,
            allowed_hosts: vec![API_HOST.to_string()],
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
    let client = Client {
        http,
        api_key: api_key.map(str::trim).filter(|key| !key.is_empty()),
    };
    match command {
        PluginListCommand::Fetch(request) => {
            PluginListCommandResult::Fetch(into_result(client.fetch(&request).await))
        }
        PluginListCommand::Health(_) => {
            PluginListCommandResult::Health(into_result(client.health().await))
        }
        PluginListCommand::Account(_) => {
            PluginListCommandResult::Account(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "TMDb member accounts are not supported yet",
            )))
        }
    }
}

struct Client<'a, H> {
    http: &'a H,
    api_key: Option<&'a str>,
}

/// A v4 read access token is a JWT: three dot-separated base64url segments.
fn is_bearer_token(key: &str) -> bool {
    key.starts_with("eyJ") && key.matches('.').count() == 2
}

fn required_id(request: &ListPluginFetchRequest, key: &str) -> Result<String, PluginError> {
    let value = request
        .params
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_param(key))?;
    // Accept a pasted slug such as `1234-some-name`: TMDb ids are its digits.
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.trim_start_matches('0').is_empty() {
        return Err(invalid_config(format!("{key} must be a TMDb numeric id")));
    }
    Ok(digits)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KindFilter {
    Movie,
    Series,
    All,
}

impl KindFilter {
    fn parse(
        value: Option<&String>,
        default: KindFilter,
        allow_all: bool,
    ) -> Result<Self, PluginError> {
        match value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") => Ok(default),
            Some("movie" | "movies") => Ok(Self::Movie),
            Some("series" | "tv" | "show" | "shows") => Ok(Self::Series),
            Some("all") if allow_all => Ok(Self::All),
            Some(other) => Err(invalid_config(format!("unknown media kind {other}"))),
        }
    }

    fn accepts(self, kind: ListMediaKind) -> bool {
        match self {
            Self::All => true,
            Self::Movie => kind == ListMediaKind::Movie,
            Self::Series => kind == ListMediaKind::Series,
        }
    }
}

impl<H: ListHttp> Client<'_, H> {
    fn key(&self) -> Result<&str, PluginError> {
        self.api_key
            .ok_or_else(|| invalid_config("the TMDb API key is not configured"))
    }

    fn request(&self, path_and_query: &str) -> Result<PluginHttpRequest, PluginError> {
        let key = self.key()?;
        let separator = if path_and_query.contains('?') {
            '&'
        } else {
            '?'
        };
        if is_bearer_token(key) {
            let mut request = get(format!("{API_BASE}{path_and_query}"), USER_AGENT, ACCEPT);
            request
                .headers
                .insert("Authorization".to_string(), format!("Bearer {key}"));
            Ok(request)
        } else {
            Ok(get(
                format!(
                    "{API_BASE}{path_and_query}{separator}api_key={}",
                    encode_component(key)
                ),
                USER_AGENT,
                ACCEPT,
            ))
        }
    }

    async fn get_json(&self, path_and_query: &str, what: &str) -> Result<Value, PluginError> {
        let response = self.http.send(self.request(path_and_query)?).await?;
        check_tmdb_status(&response, what)?;
        json_body(&response)
    }

    async fn health(&self) -> Result<ListPluginHealthResponse, PluginError> {
        if self.api_key.is_none() {
            return Ok(ListPluginHealthResponse {
                healthy: false,
                message: Some("the TMDb API key is not configured".to_string()),
            });
        }
        match self.get_json("/configuration", "TMDb configuration").await {
            Ok(_) => Ok(ListPluginHealthResponse {
                healthy: true,
                message: None,
            }),
            Err(error) if error.code == PluginErrorCode::AuthFailed => {
                Ok(ListPluginHealthResponse {
                    healthy: false,
                    message: Some(error.public_message),
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn fetch(
        &self,
        request: &ListPluginFetchRequest,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        match request.source_type.as_str() {
            SOURCE_LIST => self.fetch_list(request).await,
            SOURCE_PERSON => self.fetch_person(request).await,
            SOURCE_COMPANY => {
                self.fetch_discover(request, PARAM_COMPANY_ID, "with_companies", "company")
                    .await
            }
            SOURCE_KEYWORD => {
                self.fetch_discover(request, PARAM_KEYWORD_ID, "with_keywords", "keyword")
                    .await
            }
            other => Err(unsupported_source(other)),
        }
    }

    async fn fetch_list(
        &self,
        request: &ListPluginFetchRequest,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let list_id = required_id(request, PARAM_LIST_ID)?;
        let page = numeric_cursor(request.page_cursor.as_deref(), 1)?.max(1);
        let body = self
            .get_json(
                &format!("/list/{list_id}?page={page}"),
                &format!("TMDb list {list_id}"),
            )
            .await?;
        let entries = body
            .get("items")
            .or_else(|| body.get("results"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let items = entries
            .iter()
            .filter_map(|entry| to_item(entry, None))
            .collect();
        Ok(paged(
            items,
            page,
            total_pages(&body),
            body.get("total_results").or_else(|| body.get("item_count")),
            json_text(body.get("name")),
            format!("{SITE_BASE}/list/{list_id}"),
        ))
    }

    async fn fetch_person(
        &self,
        request: &ListPluginFetchRequest,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let person_id = required_id(request, PARAM_PERSON_ID)?;
        let credit = match request
            .params
            .get(PARAM_CREDIT)
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("cast") => &["cast"][..],
            Some("crew") => &["crew"][..],
            Some("all") => &["cast", "crew"][..],
            Some(other) => return Err(invalid_config(format!("unknown credit type {other}"))),
        };
        let kinds = KindFilter::parse(request.params.get(PARAM_KIND), KindFilter::All, true)?;
        let body = self
            .get_json(
                &format!("/person/{person_id}?append_to_response=combined_credits"),
                &format!("TMDb person {person_id}"),
            )
            .await?;
        let credits = body.get("combined_credits");
        let mut entries: Vec<&Value> = credit
            .iter()
            .filter_map(|section| {
                credits
                    .and_then(|credits| credits.get(*section))
                    .and_then(Value::as_array)
            })
            .flatten()
            .filter(|entry| !is_appearance(entry))
            .collect();
        // Newest first, then by id, so the order is stable between syncs.
        entries.sort_by(|left, right| {
            release_date(right)
                .cmp(&release_date(left))
                .then_with(|| json_id(left.get("id")).cmp(&json_id(right.get("id"))))
        });
        let items: Vec<ListPluginItem> = entries
            .into_iter()
            .filter_map(|entry| to_item(entry, None))
            .filter(|item| item.kind_hint.is_some_and(|kind| kinds.accepts(kind)))
            .collect();
        Ok(single_page(
            dedupe_and_rank(items, 1),
            json_text(body.get("name")),
            Some(format!("{SITE_BASE}/person/{person_id}")),
            request.since_fingerprint.as_deref(),
        ))
    }

    async fn fetch_discover(
        &self,
        request: &ListPluginFetchRequest,
        param: &str,
        filter: &str,
        site_path: &str,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let id = required_id(request, param)?;
        let kind =
            match KindFilter::parse(request.params.get(PARAM_KIND), KindFilter::Movie, false)? {
                KindFilter::Series => ListMediaKind::Series,
                _ => ListMediaKind::Movie,
            };
        let (endpoint, sort) = match kind {
            ListMediaKind::Series => ("tv", "first_air_date.desc"),
            _ => ("movie", "primary_release_date.desc"),
        };
        let page = numeric_cursor(request.page_cursor.as_deref(), 1)?.max(1);
        let body = self
            .get_json(
                &format!("/discover/{endpoint}?{filter}={id}&sort_by={sort}&include_adult=false&page={page}"),
                &format!("TMDb {site_path} {id}"),
            )
            .await?;
        let items = body
            .get("results")
            .and_then(Value::as_array)
            .map(|results| {
                results
                    .iter()
                    .filter_map(|entry| to_item(entry, Some(kind)))
                    .collect()
            })
            .unwrap_or_default();
        Ok(paged(
            items,
            page,
            total_pages(&body),
            body.get("total_results"),
            None,
            format!("{SITE_BASE}/{site_path}/{id}/{endpoint}"),
        ))
    }
}

/// TMDb answers 401 both for a bad key and for a private resource. Only the
/// key codes are an authentication failure; the rest mean the resource is not
/// visible, which the host treats as not found.
fn check_tmdb_status(response: &PluginHttpResponse, what: &str) -> Result<(), PluginError> {
    if response.status == 401 {
        let code = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|body| body.get("status_code").and_then(Value::as_i64));
        return Err(match code {
            Some(code) if !TMDB_INVALID_KEY_CODES.contains(&code) => {
                not_found(format!("{what} (private, HTTP 401)"))
            }
            _ => auth_failed("TMDb rejected the API key"),
        });
    }
    check_status(response, Access::ServerKey, what)
}

fn total_pages(body: &Value) -> u32 {
    body.get("total_pages")
        .and_then(Value::as_u64)
        .map(|pages| pages.min(u64::from(u32::MAX)) as u32)
        .unwrap_or(1)
}

fn paged(
    items: Vec<ListPluginItem>,
    page: u32,
    total_pages: u32,
    total: Option<&Value>,
    list_name: Option<String>,
    list_url: String,
) -> ListPluginFetchResponse {
    let last = total_pages.clamp(1, MAX_PAGES);
    let next_cursor = (page < last).then(|| (page + 1).to_string());
    let total_hint = total
        .and_then(Value::as_u64)
        .map(|total| total.min(u64::from(MAX_PAGES * PAGE_SIZE)) as u32);
    ListPluginFetchResponse {
        items: dedupe_and_rank(items, (page - 1) * PAGE_SIZE + 1),
        next_cursor,
        list_name,
        list_url: Some(list_url),
        total_hint,
        // Multi-page sources carry no fingerprint: the host only compares the
        // first page, which cannot vouch for the rest.
        fingerprint: None,
        unchanged: false,
    }
}

fn release_date(entry: &Value) -> String {
    json_text(
        entry
            .get("release_date")
            .or_else(|| entry.get("first_air_date")),
    )
    .unwrap_or_default()
}

fn is_appearance(entry: &Value) -> bool {
    entry.get("media_type").and_then(Value::as_str) == Some("tv")
        && entry
            .get("genre_ids")
            .and_then(Value::as_array)
            .is_some_and(|genres| {
                genres
                    .iter()
                    .filter_map(Value::as_i64)
                    .any(|genre| APPEARANCE_TV_GENRES.contains(&genre))
            })
}

/// Map one TMDb result. `kind` is known for discover; lists and credits carry
/// `media_type`, and people or unknown types are skipped.
fn to_item(entry: &Value, kind: Option<ListMediaKind>) -> Option<ListPluginItem> {
    let kind = match kind {
        Some(kind) => kind,
        None => match entry.get("media_type").and_then(Value::as_str) {
            Some("movie") => ListMediaKind::Movie,
            Some("tv") => ListMediaKind::Series,
            _ => return None,
        },
    };
    let ids = Ids::default()
        .with_tmdb(json_id(entry.get("id")))
        .with_imdb(json_text(entry.get("imdb_id")));
    ids.tmdb.as_ref()?;
    let title = json_text(entry.get("title").or_else(|| entry.get("name")));
    let year = json_text(
        entry
            .get("release_date")
            .or_else(|| entry.get("first_air_date")),
    )
    .and_then(|date| year_from_date(&date));
    let mut item = build_item(&ids, Some(kind), title, year)?;
    let votes = entry
        .get("vote_count")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if votes > 0 {
        item.provider_rating = entry
            .get("vote_average")
            .and_then(Value::as_f64)
            .map(|value| ListProviderRating {
                scale: PROVIDER_TYPE.to_string(),
                value,
            });
    }
    item.genres = entry
        .get("genre_ids")
        .and_then(Value::as_array)
        .map(|genres| {
            genres
                .iter()
                .filter_map(Value::as_i64)
                .filter_map(genre_name)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    item.language = json_text(entry.get("original_language"));
    Some(item)
}

/// TMDb's movie and TV genre ids. The two tables share some ids with the same
/// name, so one table serves both.
fn genre_name(id: i64) -> Option<&'static str> {
    Some(match id {
        28 => "Action",
        12 => "Adventure",
        16 => "Animation",
        35 => "Comedy",
        80 => "Crime",
        99 => "Documentary",
        18 => "Drama",
        10751 => "Family",
        14 => "Fantasy",
        36 => "History",
        27 => "Horror",
        10402 => "Music",
        9648 => "Mystery",
        10749 => "Romance",
        878 => "Science Fiction",
        10770 => "TV Movie",
        53 => "Thriller",
        10752 => "War",
        37 => "Western",
        10759 => "Action & Adventure",
        10762 => "Kids",
        10763 => "News",
        10764 => "Reality",
        10765 => "Sci-Fi & Fantasy",
        10766 => "Soap",
        10767 => "Talk",
        10768 => "War & Politics",
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
