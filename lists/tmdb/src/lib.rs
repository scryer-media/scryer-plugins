//! TMDb list provider.
//!
//! Follows four kinds of public TMDb sources with the server's TMDb API key:
//!
//! - `list`: a public list by id, paged through `/4/list/{id}` when the
//!   server key is a v4 read access token, and through `/3/list/{id}` with a
//!   v3 API key.
//! - `person`: everything a person is credited on, from one
//!   `/3/person/{id}?append_to_response=combined_credits` read: their cast
//!   credits, their crew credits, or the crew credits of one department.
//! - `company` and `keyword`: TMDb discover filtered by the company or
//!   keyword, newest first, paged.
//!
//! A paged source longer than [`MAX_PAGES`] pages fails rather than being cut
//! short: the host would read every title past the cap as having left it.
//!
//! Parameterless charts (popular, top rated, upcoming and the rest) are served
//! by the metadata gateway and are deliberately absent here.
//!
//! The key is either a v3 API key (sent as `api_key`) or a v4 read access
//! token (sent as a bearer token). It is server configuration declared in the
//! descriptor.
//!
//! A member who links their own TMDb account can also follow five personal
//! sources through TMDb's v4 account API, read with that member's v4 user
//! access token and never with the server key:
//!
//! - `watchlist`, `favorites` and `rated`: the member's watchlist, favorites
//!   or rated titles, movies and shows, newest first, paged.
//! - `recommendations`: the titles TMDb recommends to the member, movies and
//!   shows, in TMDb's order, paged.
//! - `account_list`: one of the member's own lists, public or private, paged
//!   through `/4/list/{id}`.
//!
//! The member's credential arrives inside the request for that call only. Its
//! token goes in the `Authorization` header and nowhere else; its
//! `external_user_id` is the v4 account object id the account endpoints are
//! addressed by.

use list_provider_common::error::{
    Access, auth_failed, check_status, invalid_config, missing_param, not_found, permanent,
    unsupported_source,
};
use list_provider_common::http::{HostHttp, ListHttp, encode_component, get, json_body};
use list_provider_common::ids::{
    Ids, build_item, dedupe_and_rank, json_id, json_text, kind_str, year_from_date,
};
use list_provider_common::page::{numeric_cursor, single_page};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ListAccountExchange, ListAccountFlow, ListAccountList,
    ListAuthBadge, ListCredential, ListMediaKind, ListNoteTone, ListPluginAccountResponse,
    ListPluginFetchRequest, ListPluginFetchResponse, ListPluginHealthResponse, ListPluginItem,
    ListProviderAuth, ListProviderCapabilities, ListProviderDescriptor, ListProviderGroup,
    ListProviderItem, ListProviderNote, ListProviderRating, ListProviderTile, ListSourceParam,
    ListSourceParamType, ListUrlPattern, ListUrlPatternCapture, PluginDescriptor, PluginError,
    PluginErrorCode, PluginResult, ProviderDescriptor,
};
use serde_json::Value;

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/runtime-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/list-v1.0.0"],
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
pub const SOURCE_WATCHLIST: &str = "watchlist";
pub const SOURCE_FAVORITES: &str = "favorites";
pub const SOURCE_RATED: &str = "rated";
pub const SOURCE_RECOMMENDATIONS: &str = "recommendations";
pub const SOURCE_ACCOUNT_LIST: &str = "account_list";

pub const PARAM_LIST_ID: &str = "list_id";
pub const PARAM_PERSON_ID: &str = "person_id";
pub const PARAM_COMPANY_ID: &str = "company_id";
pub const PARAM_KEYWORD_ID: &str = "keyword_id";
pub const PARAM_CREDIT: &str = "credit";
pub const PARAM_KIND: &str = "kind";

pub const API_BASE: &str = "https://api.themoviedb.org/3";
/// TMDb's v4 API, which serves a member's account with their user access
/// token.
pub const API_V4_BASE: &str = "https://api.themoviedb.org/4";
const API_HOST: &str = "api.themoviedb.org";
const SITE_BASE: &str = "https://www.themoviedb.org";
/// TMDb's image service at full size: `{base}{file_path}`.
const IMAGE_BASE: &str = "https://image.tmdb.org/t/p/original";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/json";
/// TMDb's fixed page size for lists and discover.
const PAGE_SIZE: u32 = 20;
/// Deepest page followed: 1,980 titles, just inside the host's hundred-page
/// ceiling per sync and well inside TMDb's own 500-page limit. Following
/// movies and shows together gives each kind half of it.
pub const MAX_PAGES: u32 = 99;
/// Twelve hours, the interval the other arrs use for TMDb lists.
const DEFAULT_INTERVAL_SECONDS: u64 = 12 * 60 * 60;
/// TMDb status codes that mean the key itself is bad, as opposed to a
/// resource the key may not read.
const TMDB_INVALID_KEY_CODES: [i64; 4] = [7, 10, 30, 35];
/// TMDb's "This resource is private" status.
const TMDB_PRIVATE_RESOURCE_CODE: i64 = 39;
/// Deepest page of a member's own lists the account operation reads: 200
/// lists, in one invocation.
const MAX_ACCOUNT_LIST_PAGES: u32 = 10;
/// TV genres whose credits are appearances rather than work: talk shows and
/// news.
const APPEARANCE_TV_GENRES: [i64; 2] = [10767, 10763];
/// The crew departments a person source can be narrowed to: the `credit`
/// option, and the `department` TMDb gives each crew credit.
const CREW_DEPARTMENTS: [(&str, &str); 4] = [
    ("directing", "Directing"),
    ("production", "Production"),
    ("sound", "Sound"),
    ("writing", "Writing"),
];

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

/// A source read with the member's own linked account.
fn personal(item: ListProviderItem) -> ListProviderItem {
    ListProviderItem {
        personal: true,
        ..item
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
            summary: Some(
                "Public TMDb lists, people, companies and keywords, and members' own TMDb lists"
                    .to_string(),
            ),
            blurb: Some(
                "Follow a public TMDb list, everything a person is credited on, or \
                 everything from a company or keyword. Members who link their own TMDb \
                 account can follow their watchlist, favorites, ratings, recommendations \
                 and lists. Charts such as \
                 popular and top rated come from Scryer's metadata service instead."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#0d253f".to_string(),
                ink: "#01b4e4".to_string(),
                abbr: "TM".to_string(),
            }),
            brand_url_template: None,
            coverage: both(),
            // How a member links an account. The public sources read with the
            // server key instead, declared below as a config field and badged
            // on their group.
            auth: ListProviderAuth::MemberAccount {
                flow: ListAccountFlow::TmdbApproval,
                exchange: ListAccountExchange::Direct,
                byo_app: false,
                scopes: Vec::new(),
            },
            groups: vec![
                ListProviderGroup {
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
                                enum_param(
                                    PARAM_CREDIT,
                                    "Credits",
                                    &[
                                        "cast",
                                        "crew",
                                        "all",
                                        "directing",
                                        "production",
                                        "sound",
                                        "writing",
                                    ],
                                ),
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
                },
                ListProviderGroup {
                    label: "Your TMDb account".to_string(),
                    auth_badge: ListAuthBadge::MemberAccount,
                    items: vec![
                        personal(source_item(
                            "watchlist",
                            "Watchlist",
                            "Movies and shows on your TMDb watchlist, newest first",
                            both(),
                            SOURCE_WATCHLIST,
                            vec![enum_param(PARAM_KIND, "Media", &["all", "movie", "series"])],
                        )),
                        personal(source_item(
                            "favorites",
                            "Favorites",
                            "Movies and shows you marked as favorites on TMDb, newest first",
                            both(),
                            SOURCE_FAVORITES,
                            vec![enum_param(PARAM_KIND, "Media", &["all", "movie", "series"])],
                        )),
                        personal(source_item(
                            "rated",
                            "Rated",
                            "Movies and shows you rated on TMDb, newest first",
                            both(),
                            SOURCE_RATED,
                            vec![enum_param(PARAM_KIND, "Media", &["all", "movie", "series"])],
                        )),
                        personal(source_item(
                            "recommendations",
                            "Recommendations",
                            "Movies and shows TMDb recommends to you",
                            both(),
                            SOURCE_RECOMMENDATIONS,
                            vec![enum_param(PARAM_KIND, "Media", &["all", "movie", "series"])],
                        )),
                        personal(source_item(
                            "account-list",
                            "Your list",
                            "One of your own TMDb lists, public or private",
                            both(),
                            SOURCE_ACCOUNT_LIST,
                            vec![text_param(PARAM_LIST_ID, "List id")],
                        )),
                    ],
                },
            ],
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
                account: true,
                health: true,
                requires_member_credential: false,
            },
            config_fields: vec![ConfigFieldDef {
                key: CONFIG_API_KEY.to_string(),
                label: "TMDb API key or read access token".to_string(),
                field_type: ConfigFieldType::Password,
                required: true,
                help_text: Some(
                    "A v3 API key or a v4 API read access token from your TMDb account settings. \
                     Members can link their own TMDb accounts only with the v4 read access token."
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
        PluginListCommand::Account(request) => {
            PluginListCommandResult::Account(into_result(client.account(&request.credential).await))
        }
    }
}

struct Client<'a, H> {
    http: &'a H,
    api_key: Option<&'a str>,
}

/// A member's linked TMDb account, for one call: their v4 user access token
/// and the v4 account object id it belongs to.
#[derive(Clone, Copy)]
struct Member<'a> {
    token: &'a str,
    account_id: &'a str,
}

impl<'a> Member<'a> {
    fn from_credential(credential: Option<&'a ListCredential>) -> Result<Self, PluginError> {
        let token = credential
            .map(|credential| credential.access_token.trim())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| auth_failed("this list needs a linked TMDb account"))?;
        let account_id = credential
            .and_then(|credential| credential.external_user_id.as_deref())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| auth_failed("the linked TMDb account carries no account id"))?;
        Ok(Self { token, account_id })
    }

    /// `/account/{account_object_id}`, ready for a v4 path.
    fn account_path(self) -> String {
        format!("/account/{}", encode_component(self.account_id))
    }

    fn request(self, path_and_query: &str) -> PluginHttpRequest {
        let mut request = get(format!("{API_V4_BASE}{path_and_query}"), USER_AGENT, ACCEPT);
        request.headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        );
        request
    }
}

/// What a member call reads, which decides what a TMDb 401, 403 or 404
/// means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemberRead {
    /// The member's own watchlist, favorites, ratings, recommendations or
    /// list index: any 401 or 403 means TMDb no longer accepts the linked
    /// account, and a 404 is never the collection being gone.
    Account,
    /// A list named by id, which may be someone else's private list.
    ListById,
}

/// Where a watchlist or favorites fetch is: which kind's endpoint, which page
/// of it, and the rank the page's first title follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AccountPage {
    kind: ListMediaKind,
    page: u32,
    rank_base: u32,
}

impl AccountPage {
    /// A single kind pages by number like every other paged source. Both
    /// kinds page through movies, then shows, with a `{kind}:{page}:{rank}`
    /// cursor so the shows' ranks continue after the last movie.
    fn parse(cursor: Option<&str>, kinds: KindFilter) -> Result<Self, PluginError> {
        let single = |kind| {
            Ok(Self {
                kind,
                page: page_cursor(cursor, MAX_PAGES)?,
                rank_base: 0,
            })
        };
        match kinds {
            KindFilter::Movie => single(ListMediaKind::Movie),
            KindFilter::Series => single(ListMediaKind::Series),
            KindFilter::All => {
                let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) else {
                    return Ok(Self {
                        kind: ListMediaKind::Movie,
                        page: 1,
                        rank_base: 0,
                    });
                };
                let invalid = || permanent(format!("invalid page cursor {cursor}"));
                let parts: Vec<&str> = cursor.split(':').collect();
                let [kind, page, rank_base] = parts[..] else {
                    return Err(invalid());
                };
                let kind = match kind {
                    "movie" => ListMediaKind::Movie,
                    "series" => ListMediaKind::Series,
                    _ => return Err(invalid()),
                };
                match (page.parse::<u32>(), rank_base.parse::<u32>()) {
                    (Ok(page), Ok(rank_base)) if (1..=MAX_PAGES / 2).contains(&page) => Ok(Self {
                        kind,
                        page,
                        rank_base,
                    }),
                    _ => Err(invalid()),
                }
            }
        }
    }
}

/// A numbered page cursor, from 1 to `max_pages`. A cursor past the cap is
/// never sent: the host only hands back cursors this plugin issued.
fn page_cursor(cursor: Option<&str>, max_pages: u32) -> Result<u32, PluginError> {
    let page = numeric_cursor(cursor, 1)?.max(1);
    if page > max_pages {
        return Err(permanent(format!("invalid page cursor {page}")));
    }
    Ok(page)
}

fn chained_cursor(kind: ListMediaKind, page: u32, rank_base: u32) -> String {
    format!("{}:{page}:{rank_base}", kind_str(kind).unwrap_or("movie"))
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

    /// Whether the server key is a v4 read access token, which can read the
    /// v4 API.
    fn has_bearer_key(&self) -> bool {
        self.api_key.is_some_and(is_bearer_token)
    }

    fn request_at(
        &self,
        base: &str,
        path_and_query: &str,
    ) -> Result<PluginHttpRequest, PluginError> {
        let key = self.key()?;
        let separator = if path_and_query.contains('?') {
            '&'
        } else {
            '?'
        };
        if is_bearer_token(key) {
            let mut request = get(format!("{base}{path_and_query}"), USER_AGENT, ACCEPT);
            request
                .headers
                .insert("Authorization".to_string(), format!("Bearer {key}"));
            Ok(request)
        } else {
            Ok(get(
                format!(
                    "{base}{path_and_query}{separator}api_key={}",
                    encode_component(key)
                ),
                USER_AGENT,
                ACCEPT,
            ))
        }
    }

    async fn get_json(&self, path_and_query: &str, what: &str) -> Result<Value, PluginError> {
        self.get_json_at(API_BASE, path_and_query, what).await
    }

    async fn get_json_at(
        &self,
        base: &str,
        path_and_query: &str,
        what: &str,
    ) -> Result<Value, PluginError> {
        let response = self
            .http
            .send(self.request_at(base, path_and_query)?)
            .await?;
        check_tmdb_status(&response, what)?;
        json_body(&response)
    }

    /// A v4 read with the member's own token. The server key never goes
    /// along.
    async fn get_member_json(
        &self,
        member: Member<'_>,
        path_and_query: &str,
        what: &str,
        read: MemberRead,
    ) -> Result<Value, PluginError> {
        let response = self.http.send(member.request(path_and_query)).await?;
        check_member_status(&response, what, read)?;
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
            SOURCE_WATCHLIST => self.fetch_account_titles(request, "watchlist").await,
            SOURCE_FAVORITES => self.fetch_account_titles(request, "favorites").await,
            SOURCE_RATED => self.fetch_account_titles(request, "rated").await,
            SOURCE_RECOMMENDATIONS => self.fetch_account_titles(request, "recommendations").await,
            SOURCE_ACCOUNT_LIST => self.fetch_account_list(request).await,
            other => Err(unsupported_source(other)),
        }
    }

    /// The member's watchlist, favorites, rated titles or recommendations
    /// (`collection`). Recommendations come in TMDb's order, which takes no
    /// sort; the rest newest first. Following both kinds reads movies, then
    /// shows, each capped at half the pages. A collection past its cap fails
    /// rather than being cut short.
    async fn fetch_account_titles(
        &self,
        request: &ListPluginFetchRequest,
        collection: &str,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let member = Member::from_credential(request.credential.as_ref())?;
        let kinds = KindFilter::parse(request.params.get(PARAM_KIND), KindFilter::All, true)?;
        let position = AccountPage::parse(request.page_cursor.as_deref(), kinds)?;
        let endpoint = match position.kind {
            ListMediaKind::Series => "tv",
            _ => "movie",
        };
        let sort = if collection == "recommendations" {
            ""
        } else {
            "sort_by=created_at.desc&"
        };
        let body = self
            .get_member_json(
                member,
                &format!(
                    "{}/{endpoint}/{collection}?{sort}page={}",
                    member.account_path(),
                    position.page
                ),
                &format!("TMDb {collection}"),
                MemberRead::Account,
            )
            .await?;
        let what = format!("TMDb {collection}");
        if kinds == KindFilter::All {
            let unit = match position.kind {
                ListMediaKind::Series => "shows",
                _ => "movies",
            };
            within_cap(total_pages(&body), MAX_PAGES / 2, &what, unit)?;
        } else {
            within_cap(total_pages(&body), MAX_PAGES, &what, "titles")?;
        }
        let items = body
            .get("results")
            .and_then(Value::as_array)
            .map(|results| {
                results
                    .iter()
                    .filter_map(|entry| to_item(entry, Some(position.kind)))
                    .collect()
            })
            .unwrap_or_default();
        if kinds != KindFilter::All {
            return Ok(paged(
                items,
                position.page,
                total_pages(&body),
                body.get("total_results"),
                None,
                None,
            ));
        }

        let mut response = paged(
            items,
            position.page,
            total_pages(&body).min(MAX_PAGES / 2),
            None,
            None,
            None,
        );
        for item in &mut response.items {
            item.rank = item
                .rank
                .map(|rank| rank.saturating_add(position.rank_base));
        }
        response.next_cursor = match response.next_cursor.take() {
            Some(_) => Some(chained_cursor(
                position.kind,
                position.page.saturating_add(1),
                position.rank_base,
            )),
            None if position.kind == ListMediaKind::Movie => {
                let movies = response
                    .items
                    .last()
                    .and_then(|item| item.rank)
                    .unwrap_or_else(|| {
                        position
                            .rank_base
                            .saturating_add(page_offset(position.page))
                    });
                Some(chained_cursor(ListMediaKind::Series, 1, movies))
            }
            None => None,
        };
        Ok(response)
    }

    /// One of the member's own lists, which may be private, read with their
    /// token.
    async fn fetch_account_list(
        &self,
        request: &ListPluginFetchRequest,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let member = Member::from_credential(request.credential.as_ref())?;
        let list_id = required_id(request, PARAM_LIST_ID)?;
        let page = page_cursor(request.page_cursor.as_deref(), MAX_PAGES)?;
        let body = self
            .get_member_json(
                member,
                &format!("/list/{list_id}?page={page}"),
                &format!("TMDb list {list_id}"),
                MemberRead::ListById,
            )
            .await?;
        let entries = body
            .get("results")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let pages = list_total_pages(&body, page, entries.len(), &list_id)?;
        within_cap(pages, MAX_PAGES, "TMDb list", "titles")?;
        let items = entries
            .iter()
            .filter_map(|entry| to_item(entry, None))
            .collect();
        Ok(paged(
            items,
            page,
            pages,
            body.get("total_results").or_else(|| body.get("item_count")),
            json_text(body.get("name")),
            Some(format!("{SITE_BASE}/list/{list_id}")),
        ))
    }

    /// The identity behind a member's token and the lists they own.
    ///
    /// TMDb's v4 API has no account-details read, so the name and avatar come
    /// from the owner of the member's first list. A member without lists is
    /// named by the credential's username, or else by the account id.
    async fn account(
        &self,
        credential: &ListCredential,
    ) -> Result<ListPluginAccountResponse, PluginError> {
        let member = Member::from_credential(Some(credential))?;
        let mut owned_lists = Vec::new();
        let mut page = 1;
        loop {
            let body = self
                .get_member_json(
                    member,
                    &format!("{}/lists?page={page}", member.account_path()),
                    "TMDb account lists",
                    MemberRead::Account,
                )
                .await?;
            if let Some(results) = body.get("results").and_then(Value::as_array) {
                owned_lists.extend(results.iter().filter_map(account_list));
            }
            if page >= total_pages(&body).clamp(1, MAX_ACCOUNT_LIST_PAGES) {
                break;
            }
            page += 1;
        }

        // The owner is only a name and an avatar: a first list that cannot
        // be read leaves the member unnamed by it rather than failing the
        // link.
        let owner = match owned_lists.first() {
            Some(list) => self
                .get_member_json(
                    member,
                    &format!("/list/{}?page=1", list.id),
                    &format!("TMDb list {}", list.id),
                    MemberRead::ListById,
                )
                .await
                .ok()
                .and_then(|body| body.get("created_by").cloned())
                .filter(|owner| json_text(owner.get("id")).as_deref() == Some(member.account_id)),
            None => None,
        };
        let field = |key: &str| owner.as_ref().and_then(|owner| json_text(owner.get(key)));
        let username = field("username")
            .or_else(|| {
                credential
                    .username
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| member.account_id.to_string());
        Ok(ListPluginAccountResponse {
            external_user_id: member.account_id.to_string(),
            username,
            display_name: field("name"),
            avatar_url: field("avatar_path")
                .filter(|path| path.starts_with('/'))
                .map(|path| format!("{IMAGE_BASE}{path}")),
            owned_lists,
            statuses: Vec::new(),
        })
    }

    async fn fetch_list(
        &self,
        request: &ListPluginFetchRequest,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let list_id = required_id(request, PARAM_LIST_ID)?;
        let page = page_cursor(request.page_cursor.as_deref(), MAX_PAGES)?;
        // TMDb documents `total_pages` for a v4 list read but not for v3, so
        // a v4 read access token reads the list through v4, as Radarr does.
        // A v3 API key cannot read v4 and falls back to v3, where the list's
        // item count bounds the pages instead.
        let base = if self.has_bearer_key() {
            API_V4_BASE
        } else {
            API_BASE
        };
        let body = self
            .get_json_at(
                base,
                &format!("/list/{list_id}?page={page}"),
                &format!("TMDb list {list_id}"),
            )
            .await?;
        let entries = body
            .get("results")
            .or_else(|| body.get("items"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let pages = list_total_pages(&body, page, entries.len(), &list_id)?;
        within_cap(pages, MAX_PAGES, "TMDb list", "titles")?;
        let items = entries
            .iter()
            .filter_map(|entry| to_item(entry, None))
            .collect();
        Ok(paged(
            items,
            page,
            pages,
            body.get("total_results").or_else(|| body.get("item_count")),
            json_text(body.get("name")),
            Some(format!("{SITE_BASE}/list/{list_id}")),
        ))
    }

    async fn fetch_person(
        &self,
        request: &ListPluginFetchRequest,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let person_id = required_id(request, PARAM_PERSON_ID)?;
        // The credit sections to read, and for crew a single department to
        // keep.
        let (credit, department) = match request
            .params
            .get(PARAM_CREDIT)
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("cast") => (&["cast"][..], None),
            Some("crew") => (&["crew"][..], None),
            Some("all") => (&["cast", "crew"][..], None),
            Some(other) => match CREW_DEPARTMENTS.iter().find(|(option, _)| *option == other) {
                Some((_, department)) => (&["crew"][..], Some(*department)),
                None => return Err(invalid_config(format!("unknown credit type {other}"))),
            },
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
            .filter(|entry| {
                department.is_none_or(|department| {
                    entry.get("department").and_then(Value::as_str) == Some(department)
                })
            })
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
        let page = page_cursor(request.page_cursor.as_deref(), MAX_PAGES)?;
        let body = self
            .get_json(
                &format!("/discover/{endpoint}?{filter}={id}&sort_by={sort}&include_adult=false&page={page}"),
                &format!("TMDb {site_path} {id}"),
            )
            .await?;
        within_cap(
            total_pages(&body),
            MAX_PAGES,
            &format!("TMDb {site_path}"),
            "titles",
        )?;
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
            Some(format!("{SITE_BASE}/{site_path}/{id}/{endpoint}")),
        ))
    }
}

/// TMDb's own `status_code`, carried in an error body.
fn tmdb_status_code(response: &PluginHttpResponse) -> Option<i64> {
    serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|body| body.get("status_code").and_then(Value::as_i64))
}

/// TMDb answers 401 both for a bad key and for a private resource. Only the
/// key codes are an authentication failure; the rest mean the resource is not
/// visible, which the host treats as not found.
fn check_tmdb_status(response: &PluginHttpResponse, what: &str) -> Result<(), PluginError> {
    if response.status == 401 {
        return Err(match tmdb_status_code(response) {
            Some(code) if !TMDB_INVALID_KEY_CODES.contains(&code) => {
                not_found(format!("{what} (private, HTTP 401)"))
            }
            _ => auth_failed("TMDb rejected the API key"),
        });
    }
    check_status(response, Access::ServerKey, what)
}

/// A 401 on a member's call means TMDb no longer accepts the linked account,
/// which the host shows as an expired account to reconnect. The one exception
/// is a list read by id that TMDb calls private: that list is not this
/// member's to read, and the account is fine.
///
/// The member's own collections are addressed by their account, so they can
/// never be gone on their own: a 403 there is TMDb refusing the linked
/// account, and a 404 a failure that must not read as the collection having
/// been deleted.
fn check_member_status(
    response: &PluginHttpResponse,
    what: &str,
    read: MemberRead,
) -> Result<(), PluginError> {
    if read == MemberRead::Account {
        match response.status {
            403 => {
                return Err(auth_failed(
                    "TMDb refused the linked account access to its own titles (HTTP 403)",
                ));
            }
            404 | 410 => {
                return Err(permanent(format!(
                    "TMDb did not serve the {what} for the linked account (HTTP {})",
                    response.status
                )));
            }
            _ => {}
        }
    }
    if response.status == 401 {
        return Err(
            if read == MemberRead::ListById
                && tmdb_status_code(response) == Some(TMDB_PRIVATE_RESOURCE_CODE)
            {
                not_found(format!("{what} (private, HTTP 401)"))
            } else {
                auth_failed("TMDb no longer accepts the linked account")
            },
        );
    }
    check_status(response, Access::Public, what)
}

/// One of the member's own lists, as the account operation offers it. TMDb
/// lists may hold movies and shows alike.
fn account_list(entry: &Value) -> Option<ListAccountList> {
    let id = json_id(entry.get("id"))?;
    let name = json_text(entry.get("name")).unwrap_or_else(|| format!("List {id}"));
    Some(ListAccountList {
        id,
        name,
        kinds: vec![ListMediaKind::Movie, ListMediaKind::Series],
    })
}

fn total_pages(body: &Value) -> u32 {
    body.get("total_pages")
        .and_then(Value::as_u64)
        .map(|pages| pages.min(u64::from(u32::MAX)) as u32)
        .unwrap_or(1)
}

/// The pages a list read has, from its `total_pages`. TMDb does not document
/// `total_pages` for a v3 list, so without it the list's item count decides:
/// the list ends once every item has been read, a page that comes back empty
/// before then fails, and a full page with no count at all fails too, since
/// its end cannot be told. None of these guesses a shorter list.
fn list_total_pages(
    body: &Value,
    page: u32,
    read_on_page: usize,
    list_id: &str,
) -> Result<u32, PluginError> {
    if body.get("total_pages").and_then(Value::as_u64).is_some() {
        return Ok(total_pages(body));
    }
    let read_on_page = u64::try_from(read_on_page).unwrap_or(u64::MAX);
    let read = u64::from(page_offset(page)).saturating_add(read_on_page);
    let count = body
        .get("item_count")
        .or_else(|| body.get("total_results"))
        .and_then(Value::as_u64);
    match count {
        Some(count) if read >= count => Ok(page),
        Some(count) if read_on_page == 0 => Err(permanent(format!(
            "TMDb list {list_id} stopped after {read} of its {count} titles"
        ))),
        Some(count) => {
            let pages = count.div_ceil(u64::from(PAGE_SIZE));
            Ok(u32::try_from(pages)
                .unwrap_or(u32::MAX)
                .max(page.saturating_add(1)))
        }
        None if read_on_page >= u64::from(PAGE_SIZE) => Err(permanent(format!(
            "TMDb list {list_id} does not say how many titles it holds"
        ))),
        None => Ok(page),
    }
}

/// A source past `max_pages` fails rather than being cut short: the host
/// would read every title after the cap as having left the list. `unit`
/// names what the cap counts: titles, or one kind's movies or shows when
/// both kinds share the pages.
fn within_cap(total_pages: u32, max_pages: u32, what: &str, unit: &str) -> Result<(), PluginError> {
    if total_pages > max_pages {
        let per_kind = if unit == "titles" {
            ""
        } else {
            " when following movies and shows together"
        };
        return Err(permanent(format!(
            "the {what} has more than {} {unit}, more than Scryer follows{per_kind}",
            max_pages * PAGE_SIZE
        )));
    }
    Ok(())
}

/// The titles on the pages before `page`.
fn page_offset(page: u32) -> u32 {
    page.saturating_sub(1).saturating_mul(PAGE_SIZE)
}

fn paged(
    items: Vec<ListPluginItem>,
    page: u32,
    total_pages: u32,
    total: Option<&Value>,
    list_name: Option<String>,
    list_url: Option<String>,
) -> ListPluginFetchResponse {
    let last = total_pages.clamp(1, MAX_PAGES);
    let next_cursor = (page < last).then(|| page.saturating_add(1).to_string());
    let total_hint = total
        .and_then(Value::as_u64)
        .map(|total| total.min(u64::from(MAX_PAGES * PAGE_SIZE)) as u32);
    ListPluginFetchResponse {
        items: dedupe_and_rank(items, page_offset(page).saturating_add(1)),
        next_cursor,
        list_name,
        list_url,
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
