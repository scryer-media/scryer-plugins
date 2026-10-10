//! Simkl list provider.
//!
//! Custom lists are discovered through `/lists/user/{id}` and read through
//! `/lists/{id}` using AUTH V2. Watchlist status handling remains separate.
//!
//! Simkl keeps one library per member, split into shows, anime and movies and
//! sorted by status: watching, plan to watch, on hold, completed and dropped
//! (movies are never watching or on hold). Each source here is one of those
//! statuses, read with the member's own Simkl token from
//! `/sync/all-items/{type}/{status}`. Nothing is public, so a fetch without the
//! member's credential is refused before any request.
//!
//! Simkl asks every app that syncs on a timer to read `/sync/activities` first
//! and to read a list only when its own timestamp moved. The fetch does that:
//! the fingerprint it returns is made of the timestamps Simkl moves for the
//! source's status and for removals in each library it reads, and when they
//! match the fingerprint the host already holds the library is not read at
//! all. A library always answers in one response, so a fingerprint taken
//! before the read can only cause an extra read later, never a missed one.
//!
//! When a timestamp moved, the status is read whole but in Simkl's ID-only
//! form, `extended=ids_only`: the payload Simkl documents as cheap to pull in
//! full for finding what left a list. Simkl's richer forms, and above all
//! `full_anime_seasons`, must be paired with `date_from` on every sync after
//! the first, and a `date_from` delta names only what changed, never what
//! left, so this plugin reads neither. Without a cache of earlier syncs it
//! could not rebuild a whole status from a delta, and the host keeps no item
//! details between syncs.
//!
//! Anime seasons are separate entries on Simkl. Each stays its own item, keyed
//! by its Simkl id. The ID-only form carries no anime subtype, so the anime
//! movies of a status come from a second ID-only read narrowed with
//! `anime_type=movies`; every other anime entry is anime whose ids name a
//! series. Neither form carries Simkl's TVDB season mapping, so an anime
//! entry follows the series its ids name.
//!
//! Every request names a Simkl app by its client id in the URL, the form
//! Simkl prefers: the one the host places in the plugin config under
//! `client_id`, else the id compiled into this build. The member's token
//! travels only in the `Authorization` header and never appears in a URL or
//! an error message.

use std::collections::BTreeSet;

use list_provider_common::error::{
    Access, auth_failed, check_status, invalid_config, permanent, plugin_error, rate_limited,
    retry_after_seconds, unavailable, unsupported_source,
};
use list_provider_common::http::{HostHttp, ListHttp, encode_component, get, json_body};
use list_provider_common::ids::{
    Ids, dedupe_and_rank, external_ids, item_key, json_id, json_text, json_year, kind_str,
};
use list_provider_common::page::single_page;
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{
    ListAccountExchange, ListAccountFlow, ListAccountStatus, ListAuthBadge, ListCredential,
    ListExternalId, ListMediaKind, ListNoteTone, ListPluginAccountResponse, ListPluginFetchRequest,
    ListPluginFetchResponse, ListPluginItem, ListProviderAuth, ListProviderCapabilities,
    ListProviderDescriptor, ListProviderGroup, ListProviderItem, ListProviderNote,
    ListProviderTile, ListSourceParam, ListSourceParamType, PluginDescriptor, PluginError,
    PluginErrorCode, PluginResult, ProviderDescriptor,
};
use serde_json::Value;

mod custom_lists;

wit_bindgen::generate!({
    world: "scryer:lists/list-provider@1.0.0",
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/runtime-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/list-v1.0.0"],
    generate_all,
});

list_provider_common::list_component_main!(descriptor = descriptor, handler = handle_command,);

pub const PLUGIN_ID: &str = "simkl-list";
pub const PROVIDER_TYPE: &str = "simkl";

/// The client id compiled into this build, used only when the host's config
/// carries none. Simkl requires a client id on every request, accepts a
/// member's token only with the client id that issued it, and documents the
/// id as public and safe to ship. It stays empty: the host supplies the id of
/// the app that linked the member under [`CONFIG_CLIENT_ID`], and with
/// neither every command refuses to run.
pub const SIMKL_CLIENT_ID: &str = "";
/// The config key the host places the Simkl app client id under.
pub const CONFIG_CLIENT_ID: &str = "client_id";

pub const SOURCE_WATCHING: &str = "watching";
pub const SOURCE_PLAN_TO_WATCH: &str = "plantowatch";
pub const SOURCE_ON_HOLD: &str = "hold";
pub const SOURCE_COMPLETED: &str = "completed";
pub const SOURCE_DROPPED: &str = "dropped";
/// Uses the host's generic owned-list source and parameter contract.
pub const SOURCE_LIST: &str = "list";
pub const PARAM_LIST_ID: &str = "list_id";

/// Narrows a status to one of Simkl's libraries; absent or `all` reads every
/// library the status exists in.
pub const PARAM_TYPE: &str = "type";
pub const TYPE_ALL: &str = "all";
pub const TYPE_MOVIES: &str = "movies";
pub const TYPE_SHOWS: &str = "shows";
pub const TYPE_ANIME: &str = "anime";

pub const API_BASE: &str = "https://api.simkl.com";
const API_HOST: &str = "api.simkl.com";
/// The app name and version Simkl asks every request to carry.
const APP_NAME: &str = "scryer";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const ACCEPT: &str = "application/json";
/// Simkl's ID-only form of a library read: every id it holds for each entry
/// and nothing else.
const EXTENDED_IDS_ONLY: &str = "ids_only";
/// Narrows an anime read to anime movies, the only value Simkl accepts.
const ANIME_TYPE_MOVIES: &str = "movies";
/// The retry hint for Simkl's per-second burst limit, which Simkl says clears
/// in about a second. Its `Retry-After` is not used.
const BURST_RETRY_SECONDS: i64 = 2;
/// The AUTH V2 scope for reading a member's library. Reads are all this
/// plugin does, and Simkl asks apps that only read to ask for no more.
const SCOPE_READ: &str = "media:read";
/// The activity timestamp Simkl moves when items leave a library entirely.
const REMOVED_FROM_LIST: &str = "removed_from_list";
/// Six hours, Sonarr's and Radarr's shortest refresh for a Simkl list.
const DEFAULT_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
/// Two seconds between fetches, the spacing Sonarr and Radarr give every
/// list they read, Simkl included.
const RATE_LIMIT_SECONDS: i64 = 2;
/// The longest Simkl error name repeated in an error message.
const MAX_ERROR_NAME_LEN: usize = 40;
/// The host's external-id kind for ids that name an anime entry.
const ANIME_ID_KIND: &str = "anime";

/// A Simkl status, which is also the source type that follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Watching,
    PlanToWatch,
    OnHold,
    Completed,
    Dropped,
}

impl Status {
    pub const ALL: [Status; 5] = [
        Status::Watching,
        Status::PlanToWatch,
        Status::OnHold,
        Status::Completed,
        Status::Dropped,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Watching => SOURCE_WATCHING,
            Self::PlanToWatch => SOURCE_PLAN_TO_WATCH,
            Self::OnHold => SOURCE_ON_HOLD,
            Self::Completed => SOURCE_COMPLETED,
            Self::Dropped => SOURCE_DROPPED,
        }
    }

    pub fn parse(source_type: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.key() == source_type)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Watching => "Watching",
            Self::PlanToWatch => "Plan to watch",
            Self::OnHold => "On hold",
            Self::Completed => "Completed",
            Self::Dropped => "Dropped",
        }
    }

    fn item_id(self) -> &'static str {
        match self {
            Self::Watching => "watching",
            Self::PlanToWatch => "plan-to-watch",
            Self::OnHold => "on-hold",
            Self::Completed => "completed",
            Self::Dropped => "dropped",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Watching => "Shows and anime marked as watching",
            Self::PlanToWatch => "Movies, shows and anime marked as plan to watch",
            Self::OnHold => "Shows and anime put on hold",
            Self::Completed => "Movies, shows and anime marked as completed",
            Self::Dropped => "Movies, shows and anime marked as dropped",
        }
    }

    /// Simkl has no watching or on-hold status for movies.
    fn has_movies(self) -> bool {
        !matches!(self, Self::Watching | Self::OnHold)
    }

    fn kinds(self) -> Vec<ListMediaKind> {
        let mut kinds = Vec::with_capacity(3);
        if self.has_movies() {
            kinds.push(ListMediaKind::Movie);
        }
        kinds.push(ListMediaKind::Series);
        kinds.push(ListMediaKind::Anime);
        kinds
    }

    fn type_options(self) -> Vec<String> {
        let mut options = vec![TYPE_ALL];
        if self.has_movies() {
            options.push(TYPE_MOVIES);
        }
        options.extend([TYPE_SHOWS, TYPE_ANIME]);
        options.into_iter().map(str::to_string).collect()
    }
}

/// One of the three libraries a Simkl member's items live in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Library {
    Shows,
    Anime,
    Movies,
}

impl Library {
    /// The path segment of `/sync/all-items` and the key of its response.
    fn path(self) -> &'static str {
        match self {
            Self::Shows => TYPE_SHOWS,
            Self::Anime => TYPE_ANIME,
            Self::Movies => TYPE_MOVIES,
        }
    }

    /// The library's block in `/sync/activities`.
    fn activity_key(self) -> &'static str {
        match self {
            Self::Shows => "tv_shows",
            Self::Anime => "anime",
            Self::Movies => "movies",
        }
    }

    /// The segment that scopes a Simkl id in an item key.
    fn key_scope(self) -> &'static str {
        match self {
            Self::Shows => "show",
            Self::Anime => "anime",
            Self::Movies => "movie",
        }
    }
}

/// The libraries a status source reads, in output order.
fn libraries(
    status: Status,
    request: &ListPluginFetchRequest,
) -> Result<Vec<Library>, PluginError> {
    let requested = request
        .params
        .get(PARAM_TYPE)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    match requested.as_deref() {
        None | Some(TYPE_ALL) => {
            let mut libraries = vec![Library::Shows, Library::Anime];
            if status.has_movies() {
                libraries.push(Library::Movies);
            }
            Ok(libraries)
        }
        Some(TYPE_SHOWS) => Ok(vec![Library::Shows]),
        Some(TYPE_ANIME) => Ok(vec![Library::Anime]),
        Some(TYPE_MOVIES) if status.has_movies() => Ok(vec![Library::Movies]),
        Some(TYPE_MOVIES) => Err(invalid_config(format!(
            "Simkl has no {} list for movies",
            status.label().to_ascii_lowercase()
        ))),
        Some(other) => Err(invalid_config(format!(
            "unknown Simkl type {other}; use all, movies, shows or anime"
        ))),
    }
}

fn status_item(status: Status) -> ListProviderItem {
    ListProviderItem {
        id: status.item_id().to_string(),
        name: status.label().to_string(),
        description: Some(status.description().to_string()),
        kinds: status.kinds(),
        source_type: status.key().to_string(),
        params: vec![ListSourceParam {
            key: PARAM_TYPE.to_string(),
            label: "Type".to_string(),
            param_type: ListSourceParamType::Enum,
            options: status.type_options(),
            required: false,
        }],
        personal: true,
        default_interval_seconds: DEFAULT_INTERVAL_SECONDS,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: "Simkl".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
        sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
        socket_permissions: Vec::new(),
        provider: ProviderDescriptor::ListProvider(ListProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: Vec::new(),
            summary: Some("Simkl watchlists and custom lists for movies, shows and anime".to_string()),
            blurb: Some(
                "Link a Simkl account to follow what it is watching, plans to watch, has on \
                 hold, completed or dropped, and your custom lists (PRO/VIP). Titles are matched by the ids Simkl keeps for \
                 them, and each anime season is its own entry."
                    .to_string(),
            ),
            tile: Some(ListProviderTile {
                bg: "#000000".to_string(),
                ink: "#ffffff".to_string(),
                abbr: "SK".to_string(),
            }),
            brand_url_template: None,
            coverage: vec![
                ListMediaKind::Movie,
                ListMediaKind::Series,
                ListMediaKind::Anime,
            ],
            // Simkl's AUTH V2 authorization code flow, which requires PKCE
            // from every app. Scryer's Simkl app is a server registration, and
            // Simkl wants its secret on every token exchange and refresh, so
            // the relay holds it and renews the seven-day access tokens. Simkl
            // accepts a token only with the client id that issued it, which
            // the host passes in the config with the member's link. An
            // operator's own app is not offered.
            auth: ListProviderAuth::MemberAccount {
                flow: ListAccountFlow::AuthorizationCode { pkce: true },
                exchange: ListAccountExchange::SmgRelay,
                byo_app: false,
                scopes: vec![SCOPE_READ.to_string()],
            },
            groups: vec![ListProviderGroup {
                label: "Your Simkl library".to_string(),
                auth_badge: ListAuthBadge::MemberAccount,
                items: Status::ALL.into_iter().map(status_item)
                    .chain(std::iter::once(custom_lists::source_item())).collect(),
            }],
            notes: vec![ListProviderNote {
                tone: ListNoteTone::Info,
                text_key: "lists.note.simkl_anime_seasons".to_string(),
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
    let configured = scryer_plugin_pdk::config::get(CONFIG_CLIENT_ID)
        .ok()
        .flatten();
    run(
        &HostHttp,
        client_id(configured.as_deref(), SIMKL_CLIENT_ID),
        command,
    )
    .await
}

/// The client id to send: the host's configured one when it is set, else
/// the one built in. An empty result makes every call fail as a
/// configuration error.
pub fn client_id<'a>(configured: Option<&'a str>, built_in: &'a str) -> &'a str {
    configured
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| built_in.trim())
}

pub async fn run<H: ListHttp>(
    http: &H,
    client_id: &str,
    command: PluginListCommand,
) -> PluginListCommandResult {
    match command {
        PluginListCommand::Fetch(request) => {
            PluginListCommandResult::Fetch(into_result(fetch(http, client_id, &request).await))
        }
        PluginListCommand::Account(request) => PluginListCommandResult::Account(into_result(
            account(http, client_id, &request.credential).await,
        )),
        PluginListCommand::Health(_) => {
            PluginListCommandResult::Health(PluginResult::Err(plugin_error(
                PluginErrorCode::Unsupported,
                "Simkl lists use each member's own account; there is no server key to check",
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

pub async fn fetch<H: ListHttp>(
    http: &H,
    client_id: &str,
    request: &ListPluginFetchRequest,
) -> Result<ListPluginFetchResponse, PluginError> {
    if request.source_type == SOURCE_LIST {
        return Client::new(http, client_id, request.credential.as_ref())?
            .custom_list(request)
            .await;
    }
    let status = Status::parse(&request.source_type)
        .ok_or_else(|| unsupported_source(&request.source_type))?;
    let libraries = libraries(status, request)?;
    let client = Client::new(http, client_id, request.credential.as_ref())?;
    client.fetch(status, &libraries, request).await
}

pub async fn account<H: ListHttp>(
    http: &H,
    client_id: &str,
    credential: &ListCredential,
) -> Result<ListPluginAccountResponse, PluginError> {
    Client::new(http, client_id, Some(credential))?
        .account()
        .await
}

struct Client<'a, H> {
    http: &'a H,
    client_id: &'a str,
    token: &'a str,
}

impl<'a, H: ListHttp> Client<'a, H> {
    fn new(
        http: &'a H,
        client_id: &'a str,
        credential: Option<&'a ListCredential>,
    ) -> Result<Self, PluginError> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(invalid_config(
                "this build of the Simkl plugin has no Simkl app id yet, so it cannot reach Simkl",
            ));
        }
        let token = credential
            .map(|credential| credential.access_token.trim())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| auth_failed("a Simkl list needs the member's linked Simkl account"))?;
        Ok(Self {
            http,
            client_id,
            token,
        })
    }

    /// `query` is appended as given, after the parameters naming the app.
    fn url(&self, path: &str, query: &str) -> String {
        let mut url = format!(
            "{API_BASE}{path}?client_id={}&app-name={APP_NAME}&app-version={}",
            encode_component(self.client_id),
            encode_component(APP_VERSION),
        );
        if !query.is_empty() {
            url.push('&');
            url.push_str(query);
        }
        url
    }

    fn request(&self, path: &str, query: &str) -> PluginHttpRequest {
        let mut request = get(self.url(path, query), USER_AGENT, ACCEPT);
        request.headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        );
        request
    }

    async fn get_json(&self, path: &str, query: &str, what: &str) -> Result<Value, PluginError> {
        let response = self.http.send(self.request(path, query)).await?;
        check_simkl_status(&response, what)?;
        if response.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        json_body(&response)
    }

    async fn fetch(
        &self,
        status: Status,
        libraries: &[Library],
        request: &ListPluginFetchRequest,
    ) -> Result<ListPluginFetchResponse, PluginError> {
        let list_name = Some(status.label().to_string());
        let activities = self
            .get_json("/sync/activities", "", "Simkl activity")
            .await?;
        let fingerprint = activity_fingerprint(&activities, status, libraries);
        if let Some(fingerprint) = &fingerprint
            && request.since_fingerprint.as_deref() == Some(fingerprint.as_str())
        {
            return Ok(ListPluginFetchResponse {
                list_name,
                fingerprint: Some(fingerprint.clone()),
                unchanged: true,
                ..ListPluginFetchResponse::default()
            });
        }

        let ids_only = format!("extended={EXTENDED_IDS_ONLY}");
        let mut items = Vec::new();
        for library in libraries {
            let path = format!("/sync/all-items/{}/{}", library.path(), status.key());
            let what = format!("Simkl {} {} list", library.path(), status.key());
            let body = self.get_json(&path, &ids_only, &what).await?;
            let movies = match library {
                Library::Anime => {
                    let narrowed = format!("{ids_only}&anime_type={ANIME_TYPE_MOVIES}");
                    let body = self.get_json(&path, &narrowed, &what).await?;
                    simkl_ids(&body, *library)?
                }
                _ => BTreeSet::new(),
            };
            items.extend(library_items(&body, *library, &movies)?);
        }
        let items = dedupe_and_rank(items, 1);

        Ok(match fingerprint {
            Some(fingerprint) => ListPluginFetchResponse {
                total_hint: Some(items.len() as u32),
                items,
                next_cursor: None,
                list_name,
                list_url: None,
                fingerprint: Some(fingerprint),
                unchanged: false,
            },
            // Without usable activity timestamps the contents themselves are
            // the only safe fingerprint.
            None => single_page(items, list_name, None, request.since_fingerprint.as_deref()),
        })
    }

    async fn account(&self) -> Result<ListPluginAccountResponse, PluginError> {
        let body = self
            .get_json("/users/settings", "", "Simkl account")
            .await?;
        let external_user_id =
            json_id(body.get("account").and_then(|account| account.get("id")))
                .ok_or_else(|| permanent("Simkl did not return the member's account id"))?;
        let user = body.get("user");
        let name = json_text(user.and_then(|user| user.get("name")));
        let avatar_url = json_text(user.and_then(|user| user.get("avatar")))
            .filter(|url| url.starts_with("https://") || url.starts_with("http://"));
        let owned_lists = self.custom_lists(&external_user_id).await?;
        Ok(ListPluginAccountResponse {
            username: name.clone().unwrap_or_else(|| external_user_id.clone()),
            external_user_id,
            display_name: name,
            avatar_url,
            owned_lists,
            statuses: Status::ALL
                .into_iter()
                .map(|status| ListAccountStatus {
                    key: status.key().to_string(),
                    label: status.label().to_string(),
                    kinds: status.kinds(),
                })
                .collect(),
        })
    }
}

/// Simkl's error name from an error body's `error` field, kept only when it
/// is a plain lowercase identifier (such as `oauth2_token_required`) so
/// nothing else from the body reaches a message. Simkl's tokens all start
/// with `simkl_`, so nothing shaped like one is kept.
fn error_name(body: &Value) -> Option<&str> {
    let name = body.get("error")?.as_str()?.trim();
    let plain = !name.is_empty()
        && !name.starts_with("simkl_")
        && name.len() <= MAX_ERROR_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    plain.then_some(name)
}

fn response_error_name(response: &PluginHttpResponse) -> Option<String> {
    let body: Value = serde_json::from_slice(&response.body).ok()?;
    error_name(&body).map(str::to_string)
}

/// Map a Simkl failure onto the host's classes.
///
/// Every Simkl call here carries the member's token, so a 401 means the token
/// is expired, revoked or unknown; a V2 access token lasts seven days, and
/// renewing it needs Scryer's app secret, so it is the relay's part.
///
/// Simkl documents a 403 as a refusal that retrying cannot fix. Two of its
/// reasons, `insufficient_scope` and `oauth2_token_required`, are fixed only
/// by the member authorizing Scryer's app again; the rest are not the
/// member's to fix.
///
/// A 400 `max_items` is Simkl refusing to build a response that large;
/// retrying the same request gets the same answer.
///
/// A 412 is Simkl refusing Scryer's app itself: a wrong or suspended client
/// id, or a throttling block that lifts with time.
///
/// Simkl answers two different limits with a 429 and names which in the
/// body. `rate_limit` is the per-second burst limit, which clears in about a
/// second, so its `Retry-After` is not used. `user_limit_exceeded` is this
/// member's daily allowance and `app_limit_exceeded` the app's; both carry
/// `Retry-After` with the seconds until Simkl resets them at midnight US
/// Eastern. The host's rate-limited class pauses only the list that hit the
/// limit, which keeps a member's spent allowance from reading as Simkl being
/// down. Anything else follows the shared mapping.
fn check_simkl_status(response: &PluginHttpResponse, what: &str) -> Result<(), PluginError> {
    let status = response.status;
    let name = response_error_name(response);
    let detail = match &name {
        Some(name) => format!("HTTP {status}, {name}"),
        None => format!("HTTP {status}"),
    };
    match status {
        400 if name.as_deref() == Some("max_items") => Err(permanent(format!(
            "Simkl will not send the {what} because it is too large to build ({detail}); \
             Scryer already asks for one type and status at a time in the ID-only form, so \
             this list cannot be followed in full until Simkl raises its limit"
        ))),
        401 => Err(auth_failed(format!(
            "Simkl rejected the linked account ({detail})"
        ))),
        403 if matches!(
            name.as_deref(),
            Some("insufficient_scope" | "oauth2_token_required")
        ) =>
        {
            Err(auth_failed(format!(
                "Simkl needs the account linked to Scryer's Simkl app again ({detail})"
            )))
        }
        403 => Err(permanent(format!("Simkl refused the request ({detail})"))),
        412 => Err(unavailable(format!(
            "Simkl refused Scryer's Simkl app ({detail}); Simkl answers this for a wrong or \
             suspended app id or while it throttles the app"
        ))),
        429 => Err(simkl_rate_limited(response, name.as_deref())),
        _ => check_status(response, Access::ServerKey, what),
    }
}

fn simkl_rate_limited(response: &PluginHttpResponse, name: Option<&str>) -> PluginError {
    let daily = |whose: &str| {
        let error = rate_limited(retry_after_seconds(response));
        let seconds = error.retry_after_seconds.unwrap_or_default();
        PluginError {
            public_message: format!(
                "{whose} daily Simkl request allowance is used up (HTTP 429); Simkl resets it at \
                 midnight US Eastern, in {seconds} seconds"
            ),
            ..error
        }
    };
    match name {
        Some("rate_limit") => PluginError {
            public_message: "Simkl's per-second request limit was hit (HTTP 429, rate_limit); \
                             it clears in about a second"
                .to_string(),
            ..rate_limited(Some(BURST_RETRY_SECONDS))
        },
        Some("user_limit_exceeded") => daily("this member's"),
        Some("app_limit_exceeded") => daily("the Simkl app's"),
        _ => rate_limited(retry_after_seconds(response)),
    }
}

/// The activity timestamps a source depends on in every library it reads, or
/// `None` when any of them is missing or malformed. Simkl moves a status's
/// timestamp when items move into or out of that status, and
/// `removed_from_list` when items leave the library entirely, and its sync
/// loop rereads a list only when one of those moved. A timestamp Simkl
/// reports as null (no activity yet) is a real value: the first activity
/// changes it.
fn activity_fingerprint(
    activities: &Value,
    status: Status,
    libraries: &[Library],
) -> Option<String> {
    let mut parts = Vec::with_capacity(libraries.len() * 2);
    for library in libraries {
        let block = activities.get(library.activity_key())?;
        for field in [status.key(), REMOVED_FROM_LIST] {
            let stamp = match block.get(field)? {
                Value::String(stamp) if !stamp.trim().is_empty() => stamp.trim().to_string(),
                Value::Null => "-".to_string(),
                _ => return None,
            };
            parts.push(format!("{}.{field}={stamp}", library.path()));
        }
    }
    // v2: statuses are read in the ID-only form, so a fingerprint taken
    // from a richer read forces one fresh read.
    Some(format!(
        "simkl:v2:{APP_VERSION}:{}:{}",
        status.key(),
        parts.join(",")
    ))
}

/// The entries of one library response. Simkl leaves a library's key out
/// when it is empty and answers `{}` for an empty status.
fn library_entries(body: &Value, library: Library) -> Result<&[Value], PluginError> {
    match body {
        Value::Null => Ok(&[]),
        Value::Array(entries) if entries.is_empty() => Ok(&[]),
        Value::Object(map) => match map.get(library.path()) {
            None | Some(Value::Null) => {
                if map.contains_key("error") {
                    let name = error_name(body).unwrap_or("unnamed");
                    return Err(permanent(format!("Simkl answered with an error ({name})")));
                }
                Ok(&[])
            }
            Some(Value::Array(entries)) => Ok(entries),
            Some(_) => Err(permanent(
                "the Simkl library response has an unexpected shape",
            )),
        },
        _ => Err(permanent(
            "the Simkl library response has an unexpected shape",
        )),
    }
}

/// The entry's media block: the movie or the show, whichever it carries.
fn entry_media(entry: &Value, library: Library) -> Option<&Value> {
    match library {
        Library::Movies => entry.get("movie").or_else(|| entry.get("show")),
        Library::Shows | Library::Anime => entry.get("show").or_else(|| entry.get("movie")),
    }
}

/// The Simkl ids of every entry in one library response.
fn simkl_ids(body: &Value, library: Library) -> Result<BTreeSet<String>, PluginError> {
    Ok(library_entries(body, library)?
        .iter()
        .filter_map(|entry| json_id(entry_media(entry, library)?.get("ids")?.get("simkl")))
        .collect())
}

/// The items of one library response. `anime_movies` holds the Simkl ids of
/// the anime movies in the same status, for the anime library.
fn library_items(
    body: &Value,
    library: Library,
    anime_movies: &BTreeSet<String>,
) -> Result<Vec<ListPluginItem>, PluginError> {
    Ok(library_entries(body, library)?
        .iter()
        .filter_map(|entry| entry_item(entry, library, anime_movies))
        .collect())
}

/// What an entry is: its kind hint, and the kind its TMDb, IMDb and TVDB ids
/// describe. Anime movies are movies; every other anime entry is anime whose
/// ids name a TV series.
fn entry_kinds(
    simkl: Option<&str>,
    library: Library,
    anime_movies: &BTreeSet<String>,
) -> (ListMediaKind, ListMediaKind) {
    match library {
        Library::Movies => (ListMediaKind::Movie, ListMediaKind::Movie),
        Library::Shows => (ListMediaKind::Series, ListMediaKind::Series),
        Library::Anime if simkl.is_some_and(|simkl| anime_movies.contains(simkl)) => {
            (ListMediaKind::Movie, ListMediaKind::Movie)
        }
        Library::Anime => (ListMediaKind::Anime, ListMediaKind::Series),
    }
}

fn entry_item(
    entry: &Value,
    library: Library,
    anime_movies: &BTreeSet<String>,
) -> Option<ListPluginItem> {
    let media = entry_media(entry, library)?;
    let ids = media.get("ids");
    let id = |source: &str| ids.and_then(|ids| ids.get(source));

    let common = Ids::default()
        .with_tmdb(json_id(id("tmdb")))
        .with_imdb(json_text(id("imdb")))
        .with_tvdb(json_id(id("tvdb")));
    let simkl = json_id(id("simkl"));
    let (kind, id_kind) = entry_kinds(simkl.as_deref(), library, anime_movies);
    // The ID-only form carries no title or year; a richer one would.
    let title = json_text(media.get("title"));
    let year = json_year(media.get("year"));

    // Simkl's own id is stable even when Simkl later learns an entry's other
    // ids, and it keeps every anime season apart.
    let key = match &simkl {
        Some(simkl) => format!("simkl:{}:{simkl}", library.key_scope()),
        None => item_key(&common, Some(id_kind), title.as_deref(), year)?,
    };

    let mut external_ids = external_ids(&common, Some(id_kind));
    // Simkl's own id names an anime entry in the host's vocabulary when it
    // comes from the anime library, and the item's kind otherwise. MAL,
    // AniList, AniDB and Kitsu ids always name anime.
    let simkl_kind = match library {
        Library::Anime => Some(ANIME_ID_KIND),
        _ => kind_str(kind),
    };
    let mut push = |source: &str, value: Option<String>, kind: Option<&str>| {
        if let Some(value) = value {
            external_ids.push(ListExternalId {
                source: source.to_string(),
                kind: kind.map(str::to_string),
                id: value,
            });
        }
    };
    push("simkl", simkl, simkl_kind);
    for source in ["mal", "anilist", "anidb", "kitsu"] {
        push(source, json_id(id(source)), Some(ANIME_ID_KIND));
    }

    Some(ListPluginItem {
        item_key: key,
        kind_hint: Some(kind),
        title,
        year,
        external_ids,
        ..ListPluginItem::default()
    })
}

#[cfg(test)]
mod tests;
