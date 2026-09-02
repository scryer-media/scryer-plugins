//! Signal notifications through a signal-cli-rest-api server, as a WASI
//! Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Signal notification (`src/NzbDrone.Core/Notifications/Signal/`) is
//! four small files. `SignalProxy.SendNotification` (`SignalProxy.cs:33-64`)
//! builds `{scheme}://{host}:{port}/v2/send` from the settings, posts
//! `{message, number, recipients: [ReceiverId]}` with optional Basic auth, and
//! throws away the response. All of the client knowledge lives in
//! `SignalProxy.Test` (`SignalProxy.cs:66-121`), which is the one place Sonarr
//! maps a failure to the setting that caused it:
//!
//! * a `400` whose body contains "The plain HTTP request was sent to HTTPS
//!   port" → `UseSsl`;
//! * a `400` whose JSON `error` contains "Invalid group id" → `ReceiverId`;
//! * … "Invalid account" → `SenderNumber`;
//! * any other `400` → `Host`;
//! * a `401` → `AuthUsername` ("invalid username or password");
//! * a `WebException` → `Host`.
//!
//! On every *live* send that table is unreachable: `SendNotification` lets the
//! `HttpException` escape and the operator sees a stack trace.
//!
//! The June port copied the send and dropped the table entirely, so every
//! failure — a wrong sender number, an HTTPS port, a rejected proxy password —
//! arrived as one `HTTP 400: …` string on the delivery lane.
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * **`server_url`**, a first-class connection URL, replaces the
//!   `host`/`port`/`use_ssl` triple as the way this channel is addressed. That
//!   is not cosmetic: Scryer's loader builds a plugin's HTTP allowlist from its
//!   descriptor plus *the configuration values that parse as URLs*
//!   (`crates/scryer-plugins/src/loader.rs:3143-3181`, `host_from_url`), and an
//!   empty allowlist denies every request. A bare `host` never parses as a URL,
//!   so the June port could not reach any server at all. The legacy three keys
//!   are still read, so an existing configuration keeps working the moment its
//!   host is reachable, and the channel says plainly when it had to fall back to
//!   them;
//! * **`receiver_id` accepts a list.** signal-cli-rest-api's `recipients` is an
//!   array and Sonarr only ever fills one slot. It is now a `Tag` field, and —
//!   because the server refuses to mix recipient kinds, and refuses more than
//!   one group, in a single request (see below) — the recipients are split into
//!   the minimum number of requests and reported one `target_results` entry per
//!   recipient;
//! * **every failure names a field**, on every send and not only on Test:
//!   `use_ssl`, `receiver_id`, `sender_number`, `server_url`, `auth_username`,
//!   as `InvalidConfig`/`AuthFailed`/`UpstreamUnavailable`;
//! * **a `201` is not automatically a delivery.** `ds.SendMessageResponse`
//!   carries `errors.recipients[]` with a per-recipient `reason`; Sonarr never
//!   reads the body, so a message that reached one recipient and failed another
//!   is silently a success there;
//! * **`429` is handled.** The server answers a Signal rate limit with `429` and
//!   a `challenge_tokens` array to be replayed against
//!   `/v1/accounts/{number}/rate-limit-challenge` with a solved captcha. Sonarr
//!   has no branch for it, so the operator is told "unable to send" and nothing
//!   about the captcha they have to solve;
//! * the body is enriched per event from the structured blocks the contract
//!   carries (episode, quality, release, indexer, client, size, paths, health,
//!   version) rather than being `summary_title` + `summary_message` alone;
//! * `text_mode` is **pinned** on every request. The server applies
//!   `DEFAULT_SIGNAL_TEXT_MODE` when the field is absent, so a host-wide
//!   `styled` default would silently eat the `*`, `` ` ``, `|` and `~`
//!   characters out of release names. Sending the field always makes the
//!   rendering a property of this channel's configuration instead of the
//!   server's environment.
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")` and treats every 2xx as a delivery. This channel needs
//! neither: a `400` is one of four different settings, a `401` is the reverse
//! proxy rather than Signal, a `429` carries a captcha challenge, and a `201`
//! can carry per-recipient failures.
//!
//! # Upstream reference
//!
//! signal-cli-rest-api (<https://github.com/bbernhard/signal-cli-rest-api>),
//! read 2026-09-02 at `master`:
//!
//! * `src/api/api.go` — `SendMessageV2` is `{number, recipients[], recipient,
//!   message, base64_attachments[], sticker, mentions[], quote_timestamp,
//!   quote_author, quote_message, quote_mentions[], text_mode, edit_timestamp,
//!   notify_self, link_preview, view_once}`. `SendV2` answers **201** with
//!   `ds.SendMessageResponse` on success, `400` with `{"error": …}` for a
//!   rejected request, and `429` with `{"error", "challenge_tokens",
//!   "account"}` for `client.RateLimitErrorType`, whose message is extended with
//!   "Use the attached challenge tokens to lift the rate limit restrictions via
//!   the '/v1/accounts/{number}/rate-limit-challenge' endpoint."
//! * `src/datastructs/datastructs.go` — `SendMessageResponse{timestamp,
//!   errors?}`, `SendMessageErrors{recipients[]}`,
//!   `SendMessageError{username, number, uuid, reason}`.
//! * `src/client/client.go` — `groupPrefix = "group."`; `getRecipientType`
//!   classifies a recipient as a group (base64 that decodes to 32 bytes), a
//!   phone number, or — the fallback — a username, which JSON-RPC receives
//!   prefixed `u:`. `SendV2` refuses to mix kinds ("Signal Messenger Groups and
//!   phone numbers cannot be specified together in one request! Please split
//!   them up into multiple REST API calls.") and refuses more than one group ("A
//!   signal message cannot be sent to more than one group at once!"). `About()`
//!   returns `{versions: ["v1","v2"], build: 2, mode, version, capabilities:
//!   {"v2/send": ["quotes","mentions"]}}`.
//! * `src/utils/textstyleparser.go` — `text_mode: "styled"` parses the message
//!   for `*italic*`, `**bold**`, `***bold italic***`, `` `monospace` ``,
//!   `~strikethrough~` and `||spoiler||`, and honours a backslash escape before
//!   `*`, `` ` ``, `|` and `~`.
//! * `README.md` — `MODE` is one of `normal`, `native`, `json-rpc`,
//!   `json-rpc-native`; `DEFAULT_SIGNAL_TEXT_MODE` ("normal"/"styled") supplies
//!   `text_mode` when the request omits it. The server itself has **no**
//!   authentication: `src/main.go` registers no auth middleware, so the Basic
//!   credentials Sonarr models are for a reverse proxy in front of it.
//! * <https://support.signal.org/hc/en-us/articles/6325622209178-Text-Formatting>
//!   and <https://github.com/signalapp/Signal-Desktop/issues/724> — Signal's
//!   clients cap a message body at 2000 characters.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, PluginNotificationEpisode,
    PluginNotificationTargetResult, current_sdk_constraint,
};
use serde_json::{Map, Value, json};

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:notification/notification@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = build_descriptor,
    handler = handle_notification_command,
);

const PROVIDER_TYPE: &str = "signal";
const USER_AGENT: &str = concat!("scryer-signal-plugin/", env!("CARGO_PKG_VERSION"));

/// The default `PORT` of the signal-cli-rest-api container (README), and
/// Sonarr's placeholder (`SignalSettings.cs:25`).
const DEFAULT_PORT: i64 = 8080;

/// Signal's clients cap a message body at 2000 characters. signal-cli does not
/// enforce it, so an over-long body is accepted here and truncated by whatever
/// reads it — which is worse than truncating deliberately and saying so.
const MAX_MESSAGE_CHARS: usize = 2000;

/// The bound on upstream text quoted back into `public_message`: a proxy that
/// answers with a whole HTML page must not turn one failed notification into a
/// wall of markup.
const MAX_QUOTED_ERROR: usize = 300;

/// `groupPrefix` (`src/client/client.go`).
const GROUP_PREFIX: &str = "group.";

/// The prefix signal-cli-rest-api itself puts in front of a username when it
/// hands one to signal-cli's JSON-RPC interface (`prefixUsernameMembers`). An
/// operator who has typed it is passed through verbatim.
const USERNAME_PREFIX: &str = "u:";

const TEXT_MODE_NORMAL: &str = "normal";
const TEXT_MODE_STYLED: &str = "styled";

/// `text_mode` (`SendMessageV2.TextMode`). The stored values are the server's
/// own, so the field is forwarded rather than translated.
const TEXT_MODE_OPTIONS: &[(&str, &str)] = &[
    (TEXT_MODE_NORMAL, "Plain text"),
    (
        TEXT_MODE_STYLED,
        "Styled (Signal formatting: bold title, escaped values)",
    ),
];

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Signal".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            // The channel is a signal-cli-rest-api server, and operators name it
            // both ways. An alias costs nothing and makes an imported
            // configuration resolve.
            provider_aliases: vec!["signal-cli".to_string(), "signal-cli-rest-api".to_string()],
            // Self-hosted: there is no vendor endpoint to prefill and no host
            // set to allowlist. `server_url` is the only origin this channel
            // ever reaches, and it is what the loader turns into the allowlist.
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                // `text_mode: "styled"` — Signal's own bold/italic/monospace/
                // strikethrough/spoiler formatting, not markdown or HTML.
                supports_rich_text: true,
                // `base64_attachments` exists, but the contract carries poster
                // *URLs* and this channel uploads no bytes.
                supports_images: false,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![
                    NotificationDeliveryMode::Chat,
                    NotificationDeliveryMode::Push,
                ],
                payload_formats: vec![NotificationPayloadFormat::PlainText],
                supported_events: general_notification_events(),
                // Every event below renders distinctly, so all three of the
                // core's per-event filters are meaningful for this channel.
                event_options: NotificationEventOptions {
                    supports_upgrade_filter: true,
                    supports_delete_for_upgrade_filter: true,
                    supports_health_warning_filter: true,
                },
            },
            config_fields: config_fields(),
        }),
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "server_url",
            "Server URL",
            false,
            None,
            Some(
                "signal-cli-rest-api base URL, for example http://signal-cli:8080. Scryer builds this channel's network allowlist from configuration values that are URLs, so this is the setting that makes the server reachable.",
            ),
        ),
        // Sonarr's three connection settings. Kept because config keys are a
        // public contract; demoted to "legacy" because a bare host is not a URL
        // and therefore never reaches the loader's allowlist.
        field(
            "host",
            "Host (legacy)",
            ConfigFieldType::String,
            false,
            None,
            Some("Superseded by Server URL. Used only when Server URL is empty."),
        ),
        field(
            "port",
            "Port (legacy)",
            ConfigFieldType::Number,
            false,
            Some("8080"),
            Some("Superseded by Server URL. Used only when Server URL is empty."),
        ),
        field(
            "use_ssl",
            "Use SSL (legacy)",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Superseded by Server URL. Used only when Server URL is empty."),
        ),
        field(
            "sender_number",
            "Sender Number",
            ConfigFieldType::String,
            true,
            None,
            Some(
                "The number registered with signal-cli, in international format, for example +15550001111.",
            ),
        ),
        field(
            "receiver_id",
            "Recipients",
            ConfigFieldType::Tag,
            true,
            None,
            Some(
                "One or more recipients: phone numbers in international format, group ids (group.<id>), or Signal usernames. Groups are sent one request at a time because the server refuses to mix recipient kinds.",
            ),
        ),
        field(
            "auth_username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "HTTP Basic username. signal-cli-rest-api has no authentication of its own; this is for a reverse proxy in front of it.",
            ),
        ),
        field(
            "auth_password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            Some("HTTP Basic password, required whenever a username is set."),
        ),
        select_field(
            "text_mode",
            "Text Mode",
            Some(TEXT_MODE_NORMAL),
            TEXT_MODE_OPTIONS,
        ),
        field(
            "notify_self",
            "Notify Self",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Also raise the notification on the sending account's own devices. Useful when the sender number is also the recipient.",
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Recipients
// ---------------------------------------------------------------------------

/// `getRecipientType` (`src/client/client.go`).
///
/// The classification is not cosmetic: `SendV2` refuses a request whose
/// recipients are of more than one kind, and refuses more than one group in a
/// request, so this is what decides how many requests a send becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecipientKind {
    Group,
    Number,
    Username,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Recipient {
    value: String,
    kind: RecipientKind,
}

fn classify_recipient(value: &str) -> RecipientKind {
    if value.starts_with(GROUP_PREFIX) {
        return RecipientKind::Group;
    }
    if value.starts_with(USERNAME_PREFIX) {
        return RecipientKind::Username;
    }
    if is_phone_number(value) {
        return RecipientKind::Number;
    }
    // The server's own fallback.
    RecipientKind::Username
}

/// `utils.IsPhoneNumber`: signal-cli wants an international number, so a leading
/// `+` and digits. Separators an operator might paste are tolerated here and
/// stripped before the check, but the value itself is forwarded verbatim.
fn is_phone_number(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('+') else {
        return false;
    };
    let digits = rest
        .chars()
        .filter(|character| !matches!(character, ' ' | '-' | '(' | ')' | '.'))
        .collect::<Vec<_>>();
    !digits.is_empty() && digits.iter().all(char::is_ascii_digit)
}

/// Split the recipients into the fewest requests the server will accept.
///
/// Numbers travel together, usernames travel together, and every group gets its
/// own request. Order is the operator's, so the requests — and therefore the
/// `target_results` — are deterministic.
fn batch_recipients(recipients: &[Recipient]) -> Vec<Vec<Recipient>> {
    let mut batches: Vec<Vec<Recipient>> = Vec::new();
    let mut numbers: Option<usize> = None;
    let mut usernames: Option<usize> = None;

    for recipient in recipients {
        match recipient.kind {
            RecipientKind::Group => batches.push(vec![recipient.clone()]),
            RecipientKind::Number => match numbers {
                Some(index) => batches[index].push(recipient.clone()),
                None => {
                    numbers = Some(batches.len());
                    batches.push(vec![recipient.clone()]);
                }
            },
            RecipientKind::Username => match usernames {
                Some(index) => batches[index].push(recipient.clone()),
                None => {
                    usernames = Some(batches.len());
                    batches.push(vec![recipient.clone()]);
                }
            },
        }
    }

    batches
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Everything the renderer and the sender need, resolved and validated once per
/// send so every builder below is a pure function of `(request, settings)` and
/// therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    /// Absolute origin with no trailing slash.
    base_url: String,
    /// Whether `base_url` was assembled from the legacy `host`/`port`/`use_ssl`
    /// keys rather than read from `server_url`.
    legacy_connection: bool,
    sender_number: String,
    recipients: Vec<Recipient>,
    /// A ready `Authorization` header value, or `None`.
    auth: Option<String>,
    text_mode: String,
    notify_self: bool,
}

impl Settings {
    /// `strict` is the Test-time posture, mirroring Sonarr's split between its
    /// settings validator (checked when the form is saved) and its proxy (which
    /// checks nothing on a live send). Rules the server itself will enforce are
    /// errors either way; the rules that are only *probably* wrong — a sender
    /// number that is not in international format, half a credential pair — are
    /// refused at Test time and degraded to a warning on a live send, because a
    /// guess about a setting is never worth losing a notification over.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let mut warnings = Vec::new();

        let (base_url, legacy_connection) = resolve_base_url(
            config_value("server_url").as_deref(),
            config_value("host").as_deref(),
            config_value("port").as_deref(),
            config_bool("use_ssl"),
            &mut warnings,
        )?;

        let sender_number = validated_sender_number(
            &required_config("sender_number").map_err(config_error)?,
            strict,
            &mut warnings,
        )?;

        let recipients = validated_recipients(&config_csv("receiver_id"))?;

        let auth = resolve_auth(
            config_value("auth_username").as_deref(),
            config_value("auth_password").as_deref(),
            strict,
            &mut warnings,
        )?;

        let text_mode = validated_text_mode(config_value("text_mode").as_deref())?;

        Ok((
            Self {
                base_url,
                legacy_connection,
                sender_number,
                recipients,
                auth,
                text_mode,
                notify_self: config_bool("notify_self"),
            },
            warnings,
        ))
    }
}

/// The one thing this channel has to get right before anything else: an origin
/// Scryer will actually let it reach.
///
/// `allowed_hosts_for_descriptor` (`crates/scryer-plugins/src/loader.rs:3143`)
/// unions the descriptor's static hosts with the hostname of every configuration
/// value that `url::Url::parse` accepts *and* that has a host. This channel
/// declares no static host, so `server_url` is the only thing that can put an
/// origin in the allowlist — and an empty allowlist denies every request
/// (`plugin_http_host.rs:772-797`). Sonarr's `Host` is a bare hostname and never
/// parses, which is why it is a fallback here rather than the primary setting.
fn resolve_base_url(
    server_url: Option<&str>,
    host: Option<&str>,
    port: Option<&str>,
    use_ssl: bool,
    warnings: &mut Vec<String>,
) -> Result<(String, bool), PluginError> {
    if let Some(server_url) = server_url {
        if host.is_some() {
            warnings.push(
                "both server_url and the legacy host setting are configured; server_url is used"
                    .to_string(),
            );
        }
        return Ok((normalized_url(server_url, "server_url")?, false));
    }

    let Some(host) = host else {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "signal has no server address: set server_url to the signal-cli-rest-api base URL, for example http://signal-cli:8080 (the legacy host and port settings are also accepted)"
                .to_string(),
            None,
        ));
    };

    // An operator who pasted a URL into the legacy field meant a URL. Take it,
    // and say so, rather than building `http://https://signal.example:8080`.
    if host.contains("://") {
        warnings.push(format!(
            "the legacy host setting holds a URL ({host}); it was used as the server URL, but move it to server_url so Scryer allows requests to it"
        ));
        return Ok((normalized_url(host, "host")?, false));
    }

    let port = match port {
        Some(port) => port.parse::<i64>().map_err(|error| {
            plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("port must be a whole number; got {port:?}"),
                Some(error.to_string()),
            )
        })?,
        None => DEFAULT_PORT,
    };
    if !(1..=65535).contains(&port) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("port must be between 1 and 65535; got {port}"),
            None,
        ));
    }

    let scheme = if use_ssl { "https" } else { "http" };
    warnings.push(format!(
        "this channel is configured with the legacy host and port settings. Scryer builds a plugin's network allowlist from configuration values that are URLs, so requests may be refused until server_url is set to {scheme}://{host}:{port}"
    ));
    Ok((format!("{scheme}://{host}:{port}"), true))
}

/// An absolute `http(s)` origin with no trailing slash.
fn normalized_url(raw: &str, key: &str) -> Result<String, PluginError> {
    let trimmed = raw.trim().trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "{key} must be an absolute http:// or https:// URL, for example http://signal-cli:8080"
            ),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    let authority_at = lower.find("//").map(|at| at + 2).unwrap_or(0);
    if trimmed.len() <= authority_at {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("{key} has no host, for example http://signal-cli:8080"),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    Ok(trimmed.to_string())
}

/// `SignalSettingsValidator`: `RuleFor(c => c.SenderNumber).NotEmpty()`
/// (`SignalSettings.cs:13`). The server is stricter than Sonarr — it answers
/// "Invalid account" for a number signal-cli has not registered — so the shape
/// is checked here and the registration is left to the server.
fn validated_sender_number(
    raw: &str,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<String, PluginError> {
    let value = raw.trim().to_string();
    if is_phone_number(&value) {
        return Ok(value);
    }
    let message = format!(
        "sender_number should be the registered number in international format, for example +15550001111; got {value:?}"
    );
    if strict {
        return Err(plugin_error(PluginErrorCode::InvalidConfig, message, None));
    }
    warnings.push(message);
    Ok(value)
}

/// `RuleFor(c => c.ReceiverId).NotEmpty()` (`SignalSettings.cs:14`), widened to
/// the list the server has always accepted.
fn validated_recipients(values: &[String]) -> Result<Vec<Recipient>, PluginError> {
    let mut recipients: Vec<Recipient> = Vec::new();
    for value in values {
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        if recipients
            .iter()
            .any(|recipient| recipient.value.eq_ignore_ascii_case(&value))
        {
            continue;
        }
        let kind = classify_recipient(&value);
        recipients.push(Recipient { value, kind });
    }

    if recipients.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "receiver_id needs at least one recipient: a phone number in international format, a group id (group.<id>), or a Signal username".to_string(),
            None,
        ));
    }
    Ok(recipients)
}

/// Sonarr sends the `Authorization` header only when *both* halves are set
/// (`SignalProxy.cs:47-50`), which silently drops a password-only proxy
/// credential. Half a pair is a mistake in either direction, so it is refused at
/// Test time and reported on a live send.
fn resolve_auth(
    username: Option<&str>,
    password: Option<&str>,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, PluginError> {
    match (username, password) {
        (Some(username), Some(password)) => Ok(Some(basic_auth_header(username, password))),
        (None, None) => Ok(None),
        (Some(_), None) => {
            let message =
                "auth_username is set but auth_password is empty; no Authorization header is sent"
                    .to_string();
            if strict {
                return Err(plugin_error(PluginErrorCode::InvalidConfig, message, None));
            }
            warnings.push(message);
            Ok(None)
        }
        (None, Some(_)) => {
            let message =
                "auth_password is set but auth_username is empty; no Authorization header is sent"
                    .to_string();
            if strict {
                return Err(plugin_error(PluginErrorCode::InvalidConfig, message, None));
            }
            warnings.push(message);
            Ok(None)
        }
    }
}

fn validated_text_mode(raw: Option<&str>) -> Result<String, PluginError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(TEXT_MODE_NORMAL.to_string());
    };
    let value = raw.to_ascii_lowercase();
    if TEXT_MODE_OPTIONS.iter().any(|(key, _)| *key == value) {
        return Ok(value);
    }
    Err(plugin_error(
        PluginErrorCode::InvalidConfig,
        format!(
            "text_mode must be one of {}; got {raw:?}",
            TEXT_MODE_OPTIONS
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None,
    ))
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// Backslash-escape the characters `textstyleparser.go` treats as markup.
///
/// The parser recognises `*`, `` ` ``, `|` and `~` and honours a backslash
/// before each of them, so this is exactly the set it can undo. It is only ever
/// applied when `text_mode` is `styled`; in `normal` mode the server does no
/// parsing and a backslash would be visible text.
fn style_escape(value: &str) -> String {
    const SPECIALS: [char; 4] = ['*', '`', '|', '~'];
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if SPECIALS.contains(&character) {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// Sonarr sends a fixed constant per event ("Episode Grabbed", "Import
/// Complete", …) followed by the event's prose (`Signal.cs:19-67`). Scryer's
/// dispatcher already composes an event-specific, title-bearing heading in
/// `summary_title` ("Grabbed: Example Show"), which is strictly more informative
/// as the first line of a chat message.
fn heading(req: &PluginNotificationRequest) -> String {
    let title = req.summary_title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let app = req.app.name.trim();
    if app.is_empty() {
        "Scryer".to_string()
    } else {
        app.to_string()
    }
}

/// The whole `message` field.
///
/// Sonarr's is `title\nmessage\n` (`SignalProxy.cs:35-38`, two `AppendLine`
/// calls). The trailing blank line is dropped — it is a visible empty line in a
/// chat client and carries nothing — and the structured blocks the contract
/// carries are appended as `Label: value` lines, which is the enrichment
/// Sonarr's one-prose-sentence proxy has no room for.
fn build_message(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> String {
    let styled = settings.text_mode == TEXT_MODE_STYLED;
    let text = |value: &str| {
        if styled {
            style_escape(value)
        } else {
            value.to_string()
        }
    };

    let mut lines: Vec<String> = Vec::new();

    let heading = text(&heading(req));
    lines.push(if styled {
        format!("**{heading}**")
    } else {
        heading
    });

    let summary = req.summary_message.trim();
    if !summary.is_empty() {
        lines.push(text(summary));
    }

    for (label, value) in detail_lines(req) {
        lines.push(format!("{}: {}", text(label), text(&value)));
    }

    truncate_chars(&lines.join("\n"), MAX_MESSAGE_CHARS, warnings)
}

/// Signal's clients cap a body at 2000 characters. Truncating with a visible
/// ellipsis and a `warnings` entry keeps the notification and tells the operator
/// what happened, which is the addendum's rule for a provider limit.
fn truncate_chars(value: &str, budget: usize, warnings: &mut Vec<String>) -> String {
    let length = value.chars().count();
    if length <= budget {
        return value.to_string();
    }
    warnings.push(format!(
        "the message was {length} characters and was truncated to Signal's {budget}-character limit"
    ));
    let mut out: String = value.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The structured enrichment Sonarr's Signal channel has no room for. Every line
/// is conditional on the block actually being present, so the sparse shape the
/// core sends today renders exactly the two lines the June port sent.
fn detail_lines(req: &PluginNotificationRequest) -> Vec<(&'static str, String)> {
    let mut lines: Vec<(&'static str, String)> = Vec::new();
    match req.event_type {
        NotificationEventType::Grab => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Release Group", release_group(req));
            push(&mut lines, "Indexer", indexer(req));
            push(&mut lines, "Size", size(req));
            push(&mut lines, "Client", client_name(req));
        }
        // `NotificationEventType::Download` only ever carries a FAILED download:
        // the dispatcher maps `DownloadFailed` onto it
        // (`crates/scryer-application/src/notifications/dispatcher.rs:419`,
        // `:418-447`, release-0.19.8), with the summary "Download failed: …".
        // A successful import is `ImportComplete`/`Upgrade`, so this arm renders
        // a failure and never an import path.
        NotificationEventType::Download => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Status", download_status(req));
        }
        NotificationEventType::ImportComplete
        | NotificationEventType::Upgrade
        | NotificationEventType::PostProcessingCompleted => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Release Group", release_group(req));
            push(&mut lines, "Size", size(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Destination", import_path(req));
        }
        NotificationEventType::ImportRejected => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Source", source_path(req));
            push(&mut lines, "Status", import_status(req));
        }
        NotificationEventType::Rename => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", primary_path(req));
        }
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", deleted_path(req));
            push(&mut lines, "Quality", quality(req));
        }
        NotificationEventType::TitleAdded | NotificationEventType::TitleDeleted => {
            push(&mut lines, "Path", title_path(req));
        }
        NotificationEventType::HealthIssue | NotificationEventType::HealthRestored => {
            push(&mut lines, "Check", health_source(req));
            push(&mut lines, "Detail", health_detail(req));
        }
        NotificationEventType::ApplicationUpdate => {
            push(
                &mut lines,
                "Previous Version",
                application_version(req, false),
            );
            push(&mut lines, "New Version", application_version(req, true));
        }
        NotificationEventType::ManualInteractionRequired => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Download", download_title(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Reason", manual_reason(req));
            push(&mut lines, "Link", manual_link(req));
        }
        NotificationEventType::SubtitleDownloaded | NotificationEventType::SubtitleSearchFailed => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", primary_path(req));
            push(&mut lines, "Languages", subtitle_languages(req));
        }
        NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestApproved
        | NotificationEventType::MediaRequestRejected
        | NotificationEventType::MediaRequestCanceled => {
            push(&mut lines, "Status", media_request_status(req));
            push(&mut lines, "Quality Profile", media_request_profile(req));
        }
        NotificationEventType::Test => {}
    }
    lines
}

fn push(lines: &mut Vec<(&'static str, String)>, label: &'static str, value: Option<String>) {
    if let Some(value) = value.map(|value| value.trim().to_string())
        && !value.is_empty()
    {
        lines.push((label, value));
    }
}

// ---------------------------------------------------------------------------
// Field values
// ---------------------------------------------------------------------------

/// The contract's rendered `episode.display` when the core filled it, otherwise
/// composed the way Sonarr composes an episode heading: `{season}x{episode}` and
/// the episode titles, or the air date for a daily episode.
fn episode_display(req: &PluginNotificationRequest) -> Option<String> {
    if let Some(display) = req
        .episode
        .as_ref()
        .and_then(|episode| episode.display.as_deref())
        .map(str::trim)
        .filter(|display| !display.is_empty())
    {
        return Some(display.to_string());
    }

    let episodes: Vec<&PluginNotificationEpisode> = if req.episodes.is_empty() {
        req.episode.iter().collect()
    } else {
        req.episodes.iter().collect()
    };
    let first = episodes.first().copied()?;

    let titles = episodes
        .iter()
        .filter_map(|episode| episode.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .collect::<Vec<_>>()
        .join(" + ");

    if first.episode_number.is_none()
        && let Some(air_date) = first
            .air_date
            .as_deref()
            .map(str::trim)
            .filter(|air_date| !air_date.is_empty())
    {
        return Some(if titles.is_empty() {
            air_date.to_string()
        } else {
            format!("{air_date} - {titles}")
        });
    }

    let numbers: String = episodes
        .iter()
        .filter_map(|episode| episode.episode_number.as_deref())
        .map(str::trim)
        .filter(|number| !number.is_empty())
        .map(|number| match number.parse::<u32>() {
            Ok(parsed) => format!("x{parsed:02}"),
            Err(_) => format!("x{number}"),
        })
        .collect();

    let season = first
        .season_number
        .as_deref()
        .map(str::trim)
        .filter(|season| !season.is_empty());

    match (season, numbers.is_empty(), titles.is_empty()) {
        (Some(season), false, true) => Some(format!("{season}{numbers}")),
        (Some(season), false, false) => Some(format!("{season}{numbers} - {titles}")),
        (_, _, false) => Some(titles),
        _ => None,
    }
}

fn quality(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.quality.clone()),
    )
    .or_else(|| {
        req.media_files
            .iter()
            .find_map(|file| non_empty(file.quality.clone()))
    })
}

fn release_title(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.source_title.clone()),
    )
    .or_else(|| {
        non_empty(
            req.import
                .as_ref()
                .and_then(|import| import.source_title.clone()),
        )
    })
    .or_else(|| {
        non_empty(
            req.download
                .as_ref()
                .and_then(|download| download.title.clone()),
        )
    })
}

fn release_group(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.release_group.clone()),
    )
    .or_else(|| {
        req.media_files
            .iter()
            .find_map(|file| non_empty(file.release_group.clone()))
    })
}

fn indexer(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.indexer.clone()),
    )
    .or_else(|| {
        non_empty(
            req.release
                .as_ref()
                .and_then(|release| release.provider.clone()),
        )
    })
}

fn size(req: &PluginNotificationRequest) -> Option<String> {
    let bytes = req
        .download
        .as_ref()
        .and_then(|download| download.size_bytes)
        .filter(|bytes| *bytes > 0)
        .or_else(|| {
            let total: i64 = req
                .media_files
                .iter()
                .filter_map(|file| file.size_bytes)
                .sum();
            (total > 0).then_some(total)
        })?;
    Some(format_bytes(bytes))
}

fn client_name(req: &PluginNotificationRequest) -> Option<String> {
    let download = req.download.as_ref()?;
    non_empty(download.client_name.clone()).or_else(|| non_empty(download.client_type.clone()))
}

fn download_title(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.download
            .as_ref()
            .and_then(|download| download.title.clone()),
    )
}

fn download_status(req: &PluginNotificationRequest) -> Option<String> {
    let download = req.download.as_ref()?;
    non_empty(download.status_message.clone()).or_else(|| non_empty(download.status.clone()))
}

fn import_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.import
            .as_ref()
            .and_then(|import| import.dest_path.clone()),
    )
    .or_else(|| primary_path(req))
}

fn source_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.import
            .as_ref()
            .and_then(|import| import.source_path.clone()),
    )
}

fn import_status(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.import.as_ref().and_then(|import| import.status.clone()))
}

fn primary_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.file.as_ref().and_then(|file| file.primary_path.clone())).or_else(|| {
        req.file.as_ref().and_then(|file| {
            file.media_updates
                .first()
                .map(|update| update.path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
    })
}

/// The core puts the deleted path first in `file.media_updates`; `import`
/// carries an explicit list when the delete is an upgrade replacement.
fn deleted_path(req: &PluginNotificationRequest) -> Option<String> {
    req.file
        .as_ref()
        .and_then(|file| {
            file.media_updates
                .iter()
                .find(|update| {
                    update.update_type == scryer_plugin_sdk::NotificationMediaUpdateType::Deleted
                })
                .map(|update| update.path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
        .or_else(|| {
            req.import.as_ref().and_then(|import| {
                import
                    .deleted_paths
                    .first()
                    .map(|path| path.trim().to_string())
                    .filter(|path| !path.is_empty())
            })
        })
        .or_else(|| primary_path(req))
}

fn title_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.title.as_ref().and_then(|title| title.path.clone()))
}

fn health_source(req: &PluginNotificationRequest) -> Option<String> {
    let health = req.health.as_ref()?;
    non_empty(health.code.clone()).or_else(|| non_empty(health.status.clone()))
}

fn health_detail(req: &PluginNotificationRequest) -> Option<String> {
    let health = req.health.as_ref()?;
    non_empty(health.details.clone()).or_else(|| non_empty(health.message.clone()))
}

fn application_version(req: &PluginNotificationRequest, target: bool) -> Option<String> {
    let update = req.application_update.as_ref()?;
    non_empty(if target {
        update.target_version.clone()
    } else {
        update.current_version.clone()
    })
}

fn manual_reason(req: &PluginNotificationRequest) -> Option<String> {
    let interaction = req.manual_interaction.as_ref()?;
    non_empty(interaction.reason.clone()).or_else(|| non_empty(interaction.kind.clone()))
}

/// Only an absolute http(s) link is worth putting in a chat message: a relative
/// path is not tappable on a phone.
fn manual_link(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.manual_interaction
            .as_ref()
            .and_then(|interaction| interaction.link.clone()),
    )
    .filter(|link| is_absolute_http(link))
}

fn is_absolute_http(link: &str) -> bool {
    let link = link.to_ascii_lowercase();
    link.starts_with("http://") || link.starts_with("https://")
}

fn subtitle_languages(req: &PluginNotificationRequest) -> Option<String> {
    let languages: Vec<String> = req
        .media_files
        .iter()
        .flat_map(|file| file.subtitle_languages.iter())
        .map(|language| language.trim().to_string())
        .filter(|language| !language.is_empty())
        .collect();
    (!languages.is_empty()).then(|| languages.join(", "))
}

fn media_request_status(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.media_request
            .as_ref()
            .and_then(|request| request.status.clone()),
    )
}

fn media_request_profile(req: &PluginNotificationRequest) -> Option<String> {
    let request = req.media_request.as_ref()?;
    non_empty(request.approved_quality_profile_name.clone())
        .or_else(|| non_empty(request.requested_quality_profile_name.clone()))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Sonarr's `BytesToString` rounding, so sizes read the same across channels.
fn format_bytes(bytes: i64) -> String {
    const SUFFIXES: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let magnitude = bytes.unsigned_abs() as f64;
    let place = (magnitude.log(1024.0).floor() as i32).clamp(0, 6);
    let scaled = magnitude / 1024f64.powi(place);
    let rounded = (scaled * 10.0).round() / 10.0;
    let signed = if bytes < 0 { -rounded } else { rounded };
    format!("{} {}", signed, SUFFIXES[place as usize])
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// One `POST /v2/send` body.
///
/// `text_mode` is always present: the server falls back to
/// `DEFAULT_SIGNAL_TEXT_MODE` when it is absent (`api.go`, `SendV2`), so a
/// host-wide `styled` default would otherwise reinterpret text this channel
/// escaped for `normal`. Older servers that predate the field ignore an unknown
/// JSON key, so pinning it costs nothing there.
fn build_payload(message: &str, settings: &Settings, batch: &[Recipient]) -> Value {
    let mut payload = json!({
        "message": message,
        "number": settings.sender_number,
        "recipients": batch
            .iter()
            .map(|recipient| recipient.value.clone())
            .collect::<Vec<_>>(),
        "text_mode": settings.text_mode,
    });
    if settings.notify_self {
        payload["notify_self"] = Value::Bool(true);
    }
    payload
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// What one request produced.
enum Outcome {
    Delivered {
        status: u16,
        timestamp: Option<String>,
        /// `errors.recipients[]`: who failed, and why. Everyone else in the
        /// batch succeeded.
        recipient_errors: Vec<(String, String)>,
        warnings: Vec<String>,
    },
    /// The channel itself is misconfigured. Carries the typed error so a send in
    /// which *every* request says the same thing can be reported on the typed
    /// lane naming the field.
    Misconfigured(PluginError),
    /// The provider said no, for now.
    Rejected {
        status: Option<u16>,
        detail: String,
        provider_status: String,
        retry_after_seconds: Option<i64>,
        warnings: Vec<String>,
    },
}

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    let message = build_message(req, &settings, &mut warnings);
    let url = format!("{}/v2/send", settings.base_url);
    let batches = batch_recipients(&settings.recipients);

    if req.is_test {
        warnings.extend(probe_about(&settings));
    }

    let mut target_results: Vec<PluginNotificationTargetResult> = Vec::new();
    let mut misconfigurations: Vec<PluginError> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut retry_after_seconds: Option<i64> = None;
    let mut delivery_id: Option<String> = None;

    for batch in &batches {
        let payload = build_payload(&message, &settings, batch);
        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(error) => {
                return PluginResult::Err(plugin_error(
                    PluginErrorCode::Permanent,
                    "could not encode the Signal message payload".to_string(),
                    Some(error.to_string()),
                ));
            }
        };

        match post(&url, &settings, body, req.is_test) {
            Outcome::Delivered {
                status,
                timestamp,
                recipient_errors,
                warnings: request_warnings,
            } => {
                warnings.extend(request_warnings);
                if batches.len() == 1 && batch.len() == 1 && recipient_errors.is_empty() {
                    delivery_id = timestamp;
                }
                for recipient in batch {
                    match recipient_error_for(&recipient_errors, recipient) {
                        Some(reason) => {
                            target_results.push(PluginNotificationTargetResult {
                                target: recipient.value.clone(),
                                success: false,
                                status: Some(format!("http_{status}")),
                                error: Some(reason.clone()),
                            });
                            failures.push(format!("{}: {reason}", recipient.value));
                        }
                        None => target_results.push(PluginNotificationTargetResult {
                            target: recipient.value.clone(),
                            success: true,
                            status: Some(format!("http_{status}")),
                            error: None,
                        }),
                    }
                }
                // A recipient the server named but this channel did not send to
                // is still a failure the operator must see.
                for (who, reason) in &recipient_errors {
                    if !batch
                        .iter()
                        .any(|recipient| recipient_matches(recipient, who))
                    {
                        failures.push(format!("{who}: {reason}"));
                    }
                }
            }
            Outcome::Misconfigured(error) => {
                for recipient in batch {
                    target_results.push(PluginNotificationTargetResult {
                        target: recipient.value.clone(),
                        success: false,
                        status: error
                            .debug_message
                            .clone()
                            .or_else(|| Some(format!("{:?}", error.code))),
                        error: Some(error.public_message.clone()),
                    });
                }
                failures.push(error.public_message.clone());
                misconfigurations.push(error);
            }
            Outcome::Rejected {
                status,
                detail,
                provider_status,
                retry_after_seconds: retry_after,
                warnings: request_warnings,
            } => {
                warnings.extend(request_warnings);
                if let Some(retry_after) = retry_after {
                    retry_after_seconds =
                        Some(retry_after_seconds.map_or(retry_after, |seen| seen.max(retry_after)));
                }
                for recipient in batch {
                    target_results.push(PluginNotificationTargetResult {
                        target: recipient.value.clone(),
                        success: false,
                        status: Some(
                            status
                                .map(|status| format!("http_{status}"))
                                .unwrap_or_else(|| provider_status.clone()),
                        ),
                        error: Some(detail.clone()),
                    });
                }
                failures.push(detail);
            }
        }
    }

    // Every request refused the channel's own configuration, and for the same
    // reason: that is a setting the operator must fix, not a delivery that
    // failed, so it goes on the typed lane naming the field. A partial failure —
    // which a group id that is wrong while the phone numbers are right makes
    // real — stays on the delivery lane so the recipients that did work are
    // still reported.
    if misconfigurations.len() == batches.len()
        && let Some(first) = misconfigurations.first()
        && misconfigurations
            .iter()
            .all(|error| error.code == first.code && error.public_message == first.public_message)
    {
        let mut error = first.clone();
        if batches.len() > 1 {
            error.debug_message = Some(format!(
                "every recipient failed the same way: {}",
                error.debug_message.as_deref().unwrap_or("no detail")
            ));
        }
        return PluginResult::Err(error);
    }

    let mut response = if failures.is_empty() {
        ok_response()
    } else {
        error_response(
            failures.join("; "),
            Some(format!(
                "{}/{} Signal recipients failed",
                target_results
                    .iter()
                    .filter(|result| !result.success)
                    .count(),
                target_results.len()
            )),
        )
    };
    response.delivery_id = delivery_id;
    response.retry_after_seconds = retry_after_seconds;
    response.target_results = target_results;
    response.warnings = warnings;
    PluginResult::Ok(response)
}

/// `SendMessageError` names the failed recipient by `number`, `username` or
/// `uuid`; only the first two can be matched against what was sent.
fn recipient_matches(recipient: &Recipient, who: &str) -> bool {
    if who.is_empty() {
        return false;
    }
    recipient.value.eq_ignore_ascii_case(who)
        || recipient
            .value
            .strip_prefix(USERNAME_PREFIX)
            .is_some_and(|value| value.eq_ignore_ascii_case(who))
}

fn recipient_error_for<'a>(
    recipient_errors: &'a [(String, String)],
    recipient: &Recipient,
) -> Option<&'a String> {
    recipient_errors
        .iter()
        .find(|(who, _)| recipient_matches(recipient, who))
        .map(|(_, reason)| reason)
}

fn post(url: &str, settings: &Settings, body: Vec<u8>, strict: bool) -> Outcome {
    let mut request = HttpRequest::new(url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    if let Some(auth) = &settings.auth {
        request = request.with_header("Authorization", auth);
    }

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            settings,
        ),
        Err(error) => transport_failure(&error.to_string(), settings, strict),
    }
}

/// Sonarr maps a `WebException` to its `Host` field (`SignalProxy.cs:75-79`),
/// but only inside `Test`. Scryer's host answers a refused or failed egress
/// in-band, so there are two cases: the host would not let this plugin out at
/// all — a configuration problem with a precise fix, typed on every send — and
/// the server not answering. The latter is typed `UpstreamUnavailable` on a
/// connection test (`strict`), where Sonarr would blame `Host`, and stays on the
/// delivery lane on a live send: a network blink must not be reported to the
/// operator as a broken setting. Same posture as the ntfy sibling.
fn transport_failure(error: &str, settings: &Settings, strict: bool) -> Outcome {
    let lower = error.to_ascii_lowercase();
    if lower.contains("is not allowed") || lower.contains("not permitted") {
        return Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "Scryer refused a request to {}: set server_url to that address so it is added to this channel's allowlist. Scryer derives the allowlist from configuration values that are URLs, and the legacy host setting is not one.",
                settings.base_url
            ),
            Some(error.to_string()),
        ));
    }
    let detail = format!(
        "could not reach the signal-cli-rest-api server at {}: {}",
        settings.base_url,
        ellipsize(error, MAX_QUOTED_ERROR)
    );
    if strict {
        return Outcome::Misconfigured(plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            detail,
            Some(error.to_string()),
        ));
    }
    Outcome::Rejected {
        status: None,
        detail,
        provider_status: "request_failed".to_string(),
        retry_after_seconds: None,
        warnings: Vec::new(),
    }
}

/// A Test-time-only `GET /v1/about`.
///
/// It costs one round trip and answers questions Sonarr's test cannot separate
/// from a credential problem: is `server_url` a signal-cli-rest-api server at
/// all, does it serve the `v2` API this channel posts to, and which `MODE` is it
/// running (`normal` starts a JVM per message and is measured in seconds).
///
/// Everything it finds is a warning. A probe that cannot decide must never stop
/// a delivery, and the `POST /v2/send` immediately afterwards produces the real
/// error when the server is genuinely wrong.
fn probe_about(settings: &Settings) -> Vec<String> {
    let mut request = HttpRequest::new(format!("{}/v1/about", settings.base_url))
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    if let Some(auth) = &settings.auth {
        request = request.with_header("Authorization", auth);
    }

    let Ok(response) = http::request::<Vec<u8>>(&request, None) else {
        return Vec::new();
    };

    let status = response.status_code();
    if !(200..300).contains(&status) {
        return vec![format!(
            "GET {}/v1/about answered HTTP {status}: check that the server URL points at a signal-cli-rest-api server",
            settings.base_url
        )];
    }

    about_warnings(&response.body())
}

/// `client.About` (`src/client/client.go`): `{versions, build, mode, version,
/// capabilities}`.
fn about_warnings(body: &[u8]) -> Vec<String> {
    let Ok(Value::Object(about)) = serde_json::from_slice::<Value>(body) else {
        return vec![
            "GET /v1/about did not answer with signal-cli-rest-api version information: check that the server URL points at a signal-cli-rest-api server".to_string(),
        ];
    };

    let versions: Vec<String> = about
        .get("versions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut warnings = Vec::new();
    if versions.is_empty() {
        warnings.push(
            "GET /v1/about did not report any supported API versions: check that the server URL points at a signal-cli-rest-api server".to_string(),
        );
    } else if !versions.iter().any(|version| version == "v2") {
        warnings.push(format!(
            "this server reports API versions {} and not v2; POST /v2/send may not exist on it",
            versions.join(", ")
        ));
    }

    // `MODE=normal` runs the `signal-cli` JVM once per request. It works, and it
    // is why an operator sometimes sees a notification take ten seconds.
    if about.get("mode").and_then(Value::as_str) == Some("normal") {
        warnings.push(
            "this server runs in normal mode, which starts signal-cli for every message; json-rpc mode is markedly faster for notifications".to_string(),
        );
    }

    warnings
}

/// What the server answered, in the shapes it documents.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SignalBody {
    /// `Error.error` / `SendMessageError.error`.
    error: Option<String>,
    /// `ds.SendMessageResponse.timestamp` — a string today, a number in older
    /// builds.
    timestamp: Option<String>,
    /// `ds.SendMessageResponse.errors.recipients[]`, flattened to (who, reason).
    recipient_errors: Vec<(String, String)>,
    /// `SendMessageError.challenge_tokens` on a 429.
    challenge_tokens: Vec<String>,
    /// `SendMessageError.account` on a 429.
    account: Option<String>,
    /// Whether the body parsed as a JSON object at all. A `false` here is the
    /// most useful signal this channel has: signal-cli-rest-api answers JSON on
    /// every documented status, so anything else means something that is not it
    /// answered — an auth proxy, a captive portal, or nginx's
    /// "plain HTTP request was sent to HTTPS port" page.
    is_json: bool,
    raw: String,
}

impl SignalBody {
    /// The one line of upstream text quoted back to the operator, bounded.
    fn detail(&self, status: u16) -> String {
        if let Some(text) = self
            .error
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return ellipsize(text, MAX_QUOTED_ERROR);
        }
        match self.raw.trim() {
            "" => format!("HTTP {status}"),
            raw => ellipsize(raw, MAX_QUOTED_ERROR),
        }
    }
}

fn parse_signal_body(body: &[u8]) -> SignalBody {
    let raw = String::from_utf8_lossy(body).to_string();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return SignalBody {
            raw,
            ..SignalBody::default()
        };
    };

    SignalBody {
        error: map.get("error").and_then(Value::as_str).map(str::to_string),
        timestamp: map.get("timestamp").and_then(json_scalar_string),
        recipient_errors: map
            .get("errors")
            .and_then(Value::as_object)
            .and_then(|errors| errors.get("recipients"))
            .and_then(Value::as_array)
            .map(|recipients| {
                recipients
                    .iter()
                    .filter_map(Value::as_object)
                    .map(recipient_error)
                    .collect()
            })
            .unwrap_or_default(),
        challenge_tokens: map
            .get("challenge_tokens")
            .and_then(Value::as_array)
            .map(|tokens| {
                tokens
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        account: map
            .get("account")
            .and_then(Value::as_str)
            .map(str::to_string),
        is_json: true,
        raw,
    }
}

/// `SendMessageError{username, number, uuid, reason}`.
fn recipient_error(entry: &Map<String, Value>) -> (String, String) {
    let who = ["number", "username", "uuid"]
        .into_iter()
        .find_map(|key| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("unknown recipient")
        .to_string();
    let reason = entry
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .unwrap_or("the server reported a failure with no reason")
        .to_string();
    (who, reason)
}

fn json_scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn ellipsize(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// nginx's response when an HTTP request reaches an HTTPS listener. Sonarr
/// matches the same substring (`SignalProxy.cs:86`).
const PLAIN_HTTP_TO_HTTPS: &str = "the plain http request was sent to https port";

/// Sonarr's `Test` table (`SignalProxy.cs:84-112`), lifted out of Test and onto
/// every send, plus the statuses the current API documents that Sonarr has no
/// branch for.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    settings: &Settings,
) -> Outcome {
    let answer = parse_signal_body(body);
    let detail = answer.detail(status);
    let debug = format!("HTTP {status}: {detail}");
    let mut warnings = Vec::new();

    // `SendV2` answers 201; a 200 from a proxy or an older build is just as good.
    if (200..300).contains(&status) {
        if !answer.is_json {
            warnings.push(format!(
                "the server accepted the message with HTTP {status} but did not answer like signal-cli-rest-api; check that the server URL points at one"
            ));
        }
        return Outcome::Delivered {
            status,
            timestamp: answer.timestamp.clone(),
            recipient_errors: answer.recipient_errors.clone(),
            warnings,
        };
    }

    // Checked before anything else, and against the raw body: nginx answers this
    // one as an HTML page, so it never reaches the JSON branch below.
    if answer
        .raw
        .to_ascii_lowercase()
        .contains(PLAIN_HTTP_TO_HTTPS)
    {
        return Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "the server at {} speaks HTTPS but was addressed over HTTP: use an https:// server_url (or enable use_ssl with the legacy settings)",
                settings.base_url
            ),
            Some(debug),
        ));
    }

    // A non-2xx that is not the documented error JSON did not come from
    // signal-cli-rest-api: an authenticating reverse proxy, a captive portal, or
    // an unrelated service on that origin. Naming a Signal setting there would
    // send the operator to the wrong field.
    if !answer.is_json && !(500..600).contains(&status) && status != 429 {
        return Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "the server at {} did not answer like signal-cli-rest-api (HTTP {status}): {detail}. Check the server URL and anything proxying it.",
                settings.base_url
            ),
            Some(debug),
        ));
    }

    let error_text = answer
        .error
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();

    match status {
        400 if error_text.contains("invalid group id") => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "the server rejected a recipient group id: {detail}. A group recipient is the id shown by the server's group listing, prefixed with \"{GROUP_PREFIX}\"."
            ),
            Some(debug),
        )),
        400 if error_text.contains("invalid account") => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "the server does not know the sender number {}: {detail}. Register or link that number with signal-cli first.",
                settings.sender_number
            ),
            Some(debug),
        )),
        // Bucketing exists so these never happen; if one does, this plugin built
        // the request wrong and the operator has nothing to fix.
        400 if error_text.contains("cannot be specified together")
            || error_text.contains("more than one group") =>
        {
            Outcome::Misconfigured(plugin_error(
                PluginErrorCode::Permanent,
                format!("the request this plugin built mixed recipient kinds: {detail}"),
                Some(debug),
            ))
        }
        // Sonarr's default property for a 400 is `Host` (`SignalProxy.cs:93`).
        400 => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "the signal-cli-rest-api server at {} rejected the request: {detail}",
                settings.base_url
            ),
            Some(debug),
        )),
        // "invalid username or password" (`SignalProxy.cs:107-110`). 403 is the
        // same credential from the other side. signal-cli-rest-api has no
        // authentication of its own, so this is always the proxy in front of it.
        401 | 403 => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::AuthFailed,
            format!(
                "auth_username/auth_password were rejected (HTTP {status}): {detail}. signal-cli-rest-api has no authentication of its own, so these credentials belong to whatever proxies it."
            ),
            Some(debug),
        )),
        404 => Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "{}/v2/send does not exist (HTTP 404): {detail}. Check the server URL, including any path prefix.",
                settings.base_url
            ),
            Some(debug),
        )),
        // Signal rate-limited the account. The server hands back the challenge
        // tokens to replay with a solved captcha; Sonarr has no branch for this
        // at all, so its operators only ever see "unable to send".
        429 => {
            if !answer.challenge_tokens.is_empty() {
                warnings.push(format!(
                    "Signal is rate limiting {}. Solve a captcha at https://signalcaptchas.org/challenge/generate.html and POST it with challenge token(s) {} to {}/v1/accounts/{}/rate-limit-challenge.",
                    answer.account.as_deref().unwrap_or(&settings.sender_number),
                    answer.challenge_tokens.join(", "),
                    settings.base_url,
                    answer.account.as_deref().unwrap_or(&settings.sender_number),
                ));
            }
            Outcome::Rejected {
                status: Some(429),
                detail: format!("HTTP 429: {detail}"),
                provider_status: "http_429".to_string(),
                retry_after_seconds: retry_after(headers),
                warnings,
            }
        }
        // The provider saying "not now": the delivery lane, not the
        // configuration lane.
        _ => Outcome::Rejected {
            status: Some(status),
            detail: format!("HTTP {status}: {detail}"),
            provider_status: format!("http_{status}"),
            retry_after_seconds: retry_after(headers),
            warnings,
        },
    }
}

fn retry_after(headers: &BTreeMap<String, String>) -> Option<i64> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<i64>().ok())
        .filter(|seconds| *seconds >= 0)
        .map(|seconds| seconds.max(1))
}

fn plugin_error(
    code: PluginErrorCode,
    public_message: String,
    debug_message: Option<String>,
) -> PluginError {
    PluginError {
        code,
        public_message,
        debug_message,
        retry_after_seconds: None,
        details: None,
    }
}

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per operation this plugin exports. `action` is not one of them: the descriptor advertises no action, so the host does not route
/// one here and the arm answers **in-band** with `Unsupported` rather than
/// trapping. A trap under a component costs the whole instance and replaces the
/// plugin's own diagnosis with a generic ABI failure.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(send_notification(&request))
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        NotificationMediaUpdateType, NotificationSeverity, PluginNotificationApp,
        PluginNotificationApplicationUpdate, PluginNotificationDownload,
        PluginNotificationExternalIds, PluginNotificationFile, PluginNotificationHealth,
        PluginNotificationImport, PluginNotificationManualInteraction, PluginNotificationMediaFile,
        PluginNotificationMediaUpdate, PluginNotificationRelease, PluginNotificationTitle,
    };

    fn settings() -> Settings {
        Settings {
            base_url: "http://signal.test:8080".to_string(),
            legacy_connection: false,
            sender_number: "+15550001111".to_string(),
            recipients: vec![Recipient {
                value: "+15550002222".to_string(),
                kind: RecipientKind::Number,
            }],
            auth: None,
            text_mode: TEXT_MODE_NORMAL.to_string(),
            notify_self: false,
        }
    }

    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: None,
            occurred_at: Some("2026-09-02T12:00:00+00:00".to_string()),
            correlation_id: None,
            actor: None,
            severity: Some(NotificationSeverity::Info),
            is_test: event_type == NotificationEventType::Test,
            summary_title: "Grabbed: Example Show".to_string(),
            summary_message: "Grabbed 'Example.Show.S01E01' for 'Example Show'.".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "0.19.8".to_string(),
            },
            title: None,
            episode: None,
            episodes: Vec::new(),
            release: None,
            download: None,
            import: None,
            health: None,
            file: None,
            media_files: Vec::new(),
            application_update: None,
            manual_interaction: None,
            media_request: None,
        }
    }

    fn series_title() -> PluginNotificationTitle {
        PluginNotificationTitle {
            id: Some("title-1".to_string()),
            name: "Example Show".to_string(),
            facet: "series".to_string(),
            year: Some(2024),
            slug: None,
            path: Some("/media/TV/Example Show".to_string()),
            overview: None,
            sort_title: None,
            background_url: None,
            poster_url: Some("https://images.test/poster.jpg".to_string()),
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds::default(),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn number(value: &str) -> Recipient {
        Recipient {
            value: value.to_string(),
            kind: RecipientKind::Number,
        }
    }

    // -----------------------------------------------------------------
    // Descriptor
    // -----------------------------------------------------------------

    #[test]
    fn descriptor_keeps_every_june_config_key_and_adds_the_connection_url() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("signal must describe a notification provider");
        };

        let by_key = |key: &str| {
            notification
                .config_fields
                .iter()
                .find(|field| field.key == key)
                .unwrap_or_else(|| panic!("{key} must stay a configuration key"))
        };

        for key in [
            "host",
            "port",
            "use_ssl",
            "sender_number",
            "receiver_id",
            "auth_username",
            "auth_password",
        ] {
            let _ = by_key(key);
        }

        // M1: one recipient becomes a list, without renaming the key.
        assert_eq!(by_key("receiver_id").field_type, ConfigFieldType::Tag);
        assert!(by_key("receiver_id").required);

        // The setting that makes this channel reachable at all.
        assert_eq!(by_key("server_url").field_type, ConfigFieldType::String);
        assert_eq!(
            by_key("server_url").role,
            Some(ConfigFieldRole::ConnectionUrl)
        );

        // M2: an explicit, validated text mode rather than the server's ambient
        // DEFAULT_SIGNAL_TEXT_MODE.
        assert_eq!(by_key("text_mode").field_type, ConfigFieldType::Select);
        assert_eq!(
            by_key("text_mode").default_value.as_deref(),
            Some(TEXT_MODE_NORMAL)
        );
        assert_eq!(
            by_key("auth_password").field_type,
            ConfigFieldType::Password
        );
    }

    #[test]
    fn descriptor_declares_a_self_hosted_channel() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("signal must describe a notification provider");
        };
        assert!(notification.allowed_hosts.is_empty());
        assert!(notification.default_base_url.is_none());
        assert!(notification.capabilities.supports_test);
        assert!(!notification.capabilities.requires_host_process);
        assert!(!notification.capabilities.requires_host_filesystem);
        assert!(
            notification
                .provider_aliases
                .contains(&"signal-cli-rest-api".to_string())
        );
    }

    // -----------------------------------------------------------------
    // Connection resolution
    // -----------------------------------------------------------------

    #[test]
    fn server_url_wins_over_the_legacy_host_settings() {
        let mut warnings = Vec::new();
        let (base_url, legacy) = resolve_base_url(
            Some("https://signal.example/"),
            Some("signal.internal"),
            Some("8080"),
            false,
            &mut warnings,
        )
        .expect("a server_url is enough");
        assert_eq!(base_url, "https://signal.example");
        assert!(!legacy);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("server_url"))
        );
    }

    #[test]
    fn the_legacy_host_settings_still_build_a_url_and_warn_about_the_allowlist() {
        let mut warnings = Vec::new();
        let (base_url, legacy) = resolve_base_url(
            None,
            Some("signal.internal"),
            Some("9090"),
            true,
            &mut warnings,
        )
        .expect("the legacy triple still resolves");
        assert_eq!(base_url, "https://signal.internal:9090");
        assert!(legacy);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("allowlist") && warning.contains("server_url")),
            "the operator has to be told why requests may be refused: {warnings:?}"
        );
    }

    #[test]
    fn the_legacy_port_defaults_to_sonarrs_placeholder_and_is_range_checked() {
        let mut warnings = Vec::new();
        let (base_url, _) =
            resolve_base_url(None, Some("signal.internal"), None, false, &mut warnings)
                .expect("port defaults");
        assert_eq!(base_url, "http://signal.internal:8080");

        let error = resolve_base_url(
            None,
            Some("signal.internal"),
            Some("eight"),
            false,
            &mut warnings,
        )
        .expect_err("a non-numeric port is a configuration error");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("port"));

        let error = resolve_base_url(
            None,
            Some("signal.internal"),
            Some("70000"),
            false,
            &mut warnings,
        )
        .expect_err("an out-of-range port is a configuration error");
        assert!(error.public_message.contains("65535"));
    }

    #[test]
    fn a_url_pasted_into_the_legacy_host_field_is_used_as_a_url() {
        let mut warnings = Vec::new();
        let (base_url, _) = resolve_base_url(
            None,
            Some("https://signal.example:8443/"),
            Some("8080"),
            false,
            &mut warnings,
        )
        .expect("a URL in the host field is still a URL");
        assert_eq!(base_url, "https://signal.example:8443");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("server_url"))
        );
    }

    #[test]
    fn no_server_address_at_all_names_both_settings() {
        let mut warnings = Vec::new();
        let error = resolve_base_url(None, None, None, false, &mut warnings)
            .expect_err("a channel with no address cannot send");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
        assert!(
            error.public_message.contains("host"),
            "the conformance suite unsets host and expects to be told: {error:?}"
        );
    }

    #[test]
    fn a_server_url_that_is_not_an_absolute_url_names_its_field() {
        let error = normalized_url("signal.example", "server_url").expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));

        let error = normalized_url("http://", "server_url").expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);

        assert_eq!(
            normalized_url("  HTTP://signal.example/  ", "server_url").unwrap(),
            "HTTP://signal.example",
            "the scheme check is case-insensitive but the value is preserved"
        );
    }

    // -----------------------------------------------------------------
    // Settings validation (H1)
    // -----------------------------------------------------------------

    #[test]
    fn a_sender_number_that_is_not_international_is_strict_at_test_time_and_a_warning_on_a_send() {
        let mut warnings = Vec::new();
        let error = validated_sender_number("15550001111", true, &mut warnings)
            .expect_err("Test time refuses it");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("sender_number"));

        let mut warnings = Vec::new();
        assert_eq!(
            validated_sender_number("15550001111", false, &mut warnings).unwrap(),
            "15550001111",
            "a live send must not be lost over a guess about the number's shape"
        );
        assert_eq!(warnings.len(), 1);

        let mut warnings = Vec::new();
        assert_eq!(
            validated_sender_number(" +1 555-000-1111 ", true, &mut warnings).unwrap(),
            "+1 555-000-1111"
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn recipients_are_deduplicated_and_an_empty_list_names_its_field() {
        let recipients = validated_recipients(&[
            "+15550002222".to_string(),
            " ".to_string(),
            "+15550002222".to_string(),
            "group.AAAA".to_string(),
        ])
        .expect("three values, two recipients");
        assert_eq!(recipients.len(), 2);
        assert_eq!(recipients[0].kind, RecipientKind::Number);
        assert_eq!(recipients[1].kind, RecipientKind::Group);

        let error = validated_recipients(&[]).expect_err("no recipient is a configuration error");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("receiver_id"));
    }

    #[test]
    fn recipients_are_classified_the_way_the_server_classifies_them() {
        assert_eq!(classify_recipient("+15550002222"), RecipientKind::Number);
        assert_eq!(
            classify_recipient("group.ckRzaEd4VmRzNnJa"),
            RecipientKind::Group
        );
        assert_eq!(classify_recipient("u:example.01"), RecipientKind::Username);
        // The server's own fallback for anything that is neither.
        assert_eq!(classify_recipient("example.01"), RecipientKind::Username);
        assert_eq!(classify_recipient("15550002222"), RecipientKind::Username);
    }

    #[test]
    fn half_a_credential_pair_is_strict_at_test_time_and_a_warning_on_a_send() {
        let mut warnings = Vec::new();
        let error = resolve_auth(Some("proxy"), None, true, &mut warnings)
            .expect_err("Test time refuses it");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("auth_password"));

        let mut warnings = Vec::new();
        assert!(
            resolve_auth(Some("proxy"), None, false, &mut warnings)
                .unwrap()
                .is_none()
        );
        assert_eq!(warnings.len(), 1);

        let mut warnings = Vec::new();
        assert_eq!(
            resolve_auth(Some("proxy"), Some("secret"), true, &mut warnings).unwrap(),
            Some(basic_auth_header("proxy", "secret")),
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn an_unknown_text_mode_names_its_field() {
        assert_eq!(validated_text_mode(None).unwrap(), TEXT_MODE_NORMAL);
        assert_eq!(validated_text_mode(Some("")).unwrap(), TEXT_MODE_NORMAL);
        assert_eq!(
            validated_text_mode(Some("Styled")).unwrap(),
            TEXT_MODE_STYLED
        );
        let error = validated_text_mode(Some("markdown")).expect_err("must be refused");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("text_mode"));
    }

    // -----------------------------------------------------------------
    // Batching (M1)
    // -----------------------------------------------------------------

    #[test]
    fn numbers_travel_together_and_every_group_gets_its_own_request() {
        let recipients = vec![
            number("+15550002222"),
            Recipient {
                value: "group.AAAA".to_string(),
                kind: RecipientKind::Group,
            },
            number("+15550003333"),
            Recipient {
                value: "group.BBBB".to_string(),
                kind: RecipientKind::Group,
            },
            Recipient {
                value: "u:example.01".to_string(),
                kind: RecipientKind::Username,
            },
        ];

        let batches = batch_recipients(&recipients);
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch
                    .iter()
                    .map(|recipient| recipient.value.as_str())
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            vec![
                vec!["+15550002222", "+15550003333"],
                vec!["group.AAAA"],
                vec!["group.BBBB"],
                vec!["u:example.01"],
            ],
            "the server refuses mixed kinds and more than one group per request"
        );
    }

    #[test]
    fn a_single_recipient_is_still_a_single_request() {
        let batches = batch_recipients(&[number("+15550002222")]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    // -----------------------------------------------------------------
    // Payload
    // -----------------------------------------------------------------

    #[test]
    fn the_payload_is_sonarrs_shape_with_the_recipient_list_and_a_pinned_text_mode() {
        let settings = settings();
        let payload = build_payload("Test\nBody", &settings, &settings.recipients);
        assert_eq!(payload["message"], "Test\nBody");
        assert_eq!(payload["number"], "+15550001111");
        assert_eq!(payload["recipients"], json!(["+15550002222"]));
        assert_eq!(
            payload["text_mode"], TEXT_MODE_NORMAL,
            "the field is always sent so DEFAULT_SIGNAL_TEXT_MODE cannot reinterpret the text"
        );
        assert!(
            payload.get("notify_self").is_none(),
            "an unset option must not be sent"
        );

        let mut settings = settings;
        settings.notify_self = true;
        let payload = build_payload("Test", &settings, &settings.recipients);
        assert_eq!(payload["notify_self"], true);
    }

    // -----------------------------------------------------------------
    // Message rendering (M2)
    // -----------------------------------------------------------------

    #[test]
    fn the_sparse_shape_the_core_sends_today_renders_sonarrs_two_lines() {
        let mut warnings = Vec::new();
        let message = build_message(
            &request(NotificationEventType::Grab),
            &settings(),
            &mut warnings,
        );
        assert_eq!(
            message,
            "Grabbed: Example Show\nGrabbed 'Example.Show.S01E01' for 'Example Show'."
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_fully_populated_grab_renders_every_block_the_contract_carries() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.episode = Some(PluginNotificationEpisode {
            display: Some("1x01 - Pilot".to_string()),
            ..PluginNotificationEpisode::default()
        });
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Example.Show.S01E01.1080p".to_string()),
            quality: Some("WEBDL-1080p".to_string()),
            release_group: Some("GROUP".to_string()),
            indexer: Some("Example Indexer".to_string()),
            ..PluginNotificationRelease::default()
        });
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            size_bytes: Some(2_147_483_648),
            ..PluginNotificationDownload::default()
        });

        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);
        assert_eq!(
            message,
            "Grabbed: Example Show\n\
             Grabbed 'Example.Show.S01E01' for 'Example Show'.\n\
             Episode: 1x01 - Pilot\n\
             Quality: WEBDL-1080p\n\
             Release: Example.Show.S01E01.1080p\n\
             Release Group: GROUP\n\
             Indexer: Example Indexer\n\
             Size: 2 GB\n\
             Client: Weaver"
        );
    }

    #[test]
    fn a_download_event_renders_a_failure_and_never_an_import_path() {
        let mut req = request(NotificationEventType::Download);
        req.summary_title = "Download failed: Example Show".to_string();
        req.summary_message = "The download client reported an error.".to_string();
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            status: Some("failed".to_string()),
            status_message: Some("unpack failed".to_string()),
            ..PluginNotificationDownload::default()
        });
        req.import = Some(PluginNotificationImport {
            dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            ..PluginNotificationImport::default()
        });

        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);
        assert!(message.starts_with("Download failed: Example Show\n"));
        assert!(message.contains("Status: unpack failed"));
        assert!(
            !message.contains("Destination"),
            "Download is the dispatcher's FAILED download: {message}"
        );
    }

    #[test]
    fn each_remaining_event_renders_its_own_detail_lines() {
        let mut req = request(NotificationEventType::ImportComplete);
        req.import = Some(PluginNotificationImport {
            dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            source_path: Some("/downloads/Example.Show.S01E01.mkv".to_string()),
            status: Some("imported".to_string()),
            ..PluginNotificationImport::default()
        });
        let mut warnings = Vec::new();
        assert!(
            build_message(&req, &settings(), &mut warnings)
                .contains("Destination: /media/TV/Example Show/S01E01.mkv")
        );

        let mut req = request(NotificationEventType::FileDeleted);
        req.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Example Show/old.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });
        let mut warnings = Vec::new();
        assert!(
            build_message(&req, &settings(), &mut warnings)
                .contains("File: /media/TV/Example Show/old.mkv")
        );

        let mut req = request(NotificationEventType::HealthIssue);
        req.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable".to_string()),
            ..PluginNotificationHealth::default()
        });
        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);
        assert!(message.contains("Check: IndexerStatusCheck"));
        assert!(message.contains("Detail: Indexers unavailable"));

        let mut req = request(NotificationEventType::ApplicationUpdate);
        req.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            ..PluginNotificationApplicationUpdate::default()
        });
        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);
        assert!(message.contains("Previous Version: 0.19.7"));
        assert!(message.contains("New Version: 0.19.8"));

        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            reason: Some("Import needs a decision".to_string()),
            link: Some("https://scryer.test/activity".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);
        assert!(message.contains("Reason: Import needs a decision"));
        assert!(message.contains("Link: https://scryer.test/activity"));

        let mut req = request(NotificationEventType::SubtitleDownloaded);
        req.media_files = vec![PluginNotificationMediaFile {
            path: "/media/TV/Example Show/S01E01.mkv".to_string(),
            subtitle_languages: vec!["English".to_string(), "German".to_string()],
            ..PluginNotificationMediaFile::default()
        }];
        let mut warnings = Vec::new();
        assert!(
            build_message(&req, &settings(), &mut warnings).contains("Languages: English, German")
        );
    }

    #[test]
    fn an_unknown_event_still_renders_rather_than_failing() {
        let mut warnings = Vec::new();
        let message = build_message(
            &request(NotificationEventType::Test),
            &settings(),
            &mut warnings,
        );
        assert_eq!(
            message,
            "Grabbed: Example Show\nGrabbed 'Example.Show.S01E01' for 'Example Show'."
        );
    }

    #[test]
    fn an_empty_summary_falls_back_to_the_application_name() {
        let mut req = request(NotificationEventType::Test);
        req.summary_title = "   ".to_string();
        req.summary_message = String::new();
        let mut warnings = Vec::new();
        assert_eq!(build_message(&req, &settings(), &mut warnings), "Scryer");
    }

    #[test]
    fn styled_mode_bolds_the_heading_and_escapes_the_parsers_markup() {
        let mut settings = settings();
        settings.text_mode = TEXT_MODE_STYLED.to_string();

        let mut req = request(NotificationEventType::Grab);
        req.summary_title = "Grabbed: *Example* Show".to_string();
        req.summary_message = "Release ~GROUP~ | `raw`".to_string();

        let mut warnings = Vec::new();
        let message = build_message(&req, &settings, &mut warnings);
        assert_eq!(
            message, "**Grabbed: \\*Example\\* Show**\nRelease \\~GROUP\\~ \\| \\`raw\\`",
            "textstyleparser.go escapes *, `, | and ~ behind a backslash"
        );
    }

    #[test]
    fn normal_mode_never_escapes_because_the_server_does_not_parse() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_message = "Release ~GROUP~ | `raw`".to_string();
        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);
        assert!(message.ends_with("Release ~GROUP~ | `raw`"));
    }

    #[test]
    fn an_over_long_message_is_truncated_and_reported() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_message = "x".repeat(4000);
        let mut warnings = Vec::new();
        let message = build_message(&req, &settings(), &mut warnings);
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS);
        assert!(message.ends_with('…'));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("2000"));
    }

    // -----------------------------------------------------------------
    // Response classification (H1)
    // -----------------------------------------------------------------

    fn classify(status: u16, body: &str) -> Outcome {
        classify_response(status, &headers(&[]), body.as_bytes(), &settings())
    }

    fn misconfiguration(outcome: Outcome) -> PluginError {
        match outcome {
            Outcome::Misconfigured(error) => error,
            Outcome::Delivered { .. } => panic!("expected a configuration error, got a delivery"),
            Outcome::Rejected { detail, .. } => {
                panic!("expected a configuration error, got a rejection: {detail}")
            }
        }
    }

    #[test]
    fn a_created_message_is_a_delivery_and_carries_the_timestamp() {
        let Outcome::Delivered {
            status,
            timestamp,
            recipient_errors,
            warnings,
        } = classify(201, r#"{"timestamp":"1756000000000"}"#)
        else {
            panic!("201 is SendV2's success status");
        };
        assert_eq!(status, 201);
        assert_eq!(timestamp.as_deref(), Some("1756000000000"));
        assert!(recipient_errors.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_numeric_timestamp_from_an_older_build_is_still_read() {
        let Outcome::Delivered { timestamp, .. } = classify(201, r#"{"timestamp":1756000000000}"#)
        else {
            panic!("a delivery");
        };
        assert_eq!(timestamp.as_deref(), Some("1756000000000"));
    }

    #[test]
    fn per_recipient_errors_on_a_201_are_read_rather_than_ignored() {
        let Outcome::Delivered {
            recipient_errors, ..
        } = classify(
            201,
            r#"{"timestamp":"1","errors":{"recipients":[{"number":"+15550002222","reason":"Unregistered user"}]}}"#,
        )
        else {
            panic!("a delivery with partial failures");
        };
        assert_eq!(
            recipient_errors,
            vec![("+15550002222".to_string(), "Unregistered user".to_string())]
        );
    }

    #[test]
    fn a_success_that_does_not_look_like_signal_cli_rest_api_warns() {
        let Outcome::Delivered { warnings, .. } = classify(200, "<html>ok</html>") else {
            panic!("a 2xx is never refused over its body");
        };
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("signal-cli-rest-api"));
    }

    #[test]
    fn an_http_request_to_an_https_port_names_use_ssl() {
        // nginx answers this as an HTML page, so it never reaches the JSON
        // branch. `SignalProxy.cs:86-89`.
        let error = misconfiguration(classify(
            400,
            "<html><head><title>400 The plain HTTP request was sent to HTTPS port</title></head></html>",
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("HTTPS"));
        assert!(error.public_message.contains("use_ssl"));
    }

    #[test]
    fn an_invalid_group_id_names_receiver_id() {
        let error = misconfiguration(classify(400, r#"{"error":"Invalid group id: not-base64"}"#));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("group id"));
    }

    #[test]
    fn an_invalid_account_names_sender_number() {
        let error = misconfiguration(classify(
            400,
            r#"{"error":"Invalid account (Invalid account)"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("+15550001111"));
        assert!(error.public_message.contains("sender number"));
    }

    #[test]
    fn any_other_400_names_the_server() {
        let error = misconfiguration(classify(400, r#"{"error":"Couldn't process request"}"#));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("http://signal.test:8080"));
        assert!(error.public_message.contains("Couldn't process request"));
    }

    #[test]
    fn a_mixed_recipient_rejection_is_this_plugins_bug_not_the_operators() {
        let error = misconfiguration(classify(
            400,
            r#"{"error":"Signal Messenger Groups and phone numbers cannot be specified together in one request! Please split them up into multiple REST API calls."}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("this plugin built"));
    }

    #[test]
    fn a_401_or_403_names_the_proxy_credentials() {
        for status in [401, 403] {
            let error = misconfiguration(classify(status, r#"{"error":"unauthorized"}"#));
            assert_eq!(error.code, PluginErrorCode::AuthFailed);
            assert!(error.public_message.contains("auth_username"));
            assert!(
                error
                    .public_message
                    .contains("no authentication of its own"),
                "the operator has to be told these credentials are the proxy's"
            );
        }
    }

    #[test]
    fn a_404_names_the_server_url() {
        let error = misconfiguration(classify(404, r#"{"error":"not found"}"#));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("/v2/send"));
    }

    #[test]
    fn a_non_json_failure_blames_the_url_rather_than_a_signal_setting() {
        let error = misconfiguration(classify(407, "<html>Proxy Authentication Required</html>"));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("did not answer like"));
    }

    #[test]
    fn a_rate_limit_surfaces_the_challenge_tokens_and_the_endpoint_that_clears_it() {
        let Outcome::Rejected {
            status,
            provider_status,
            retry_after_seconds,
            warnings,
            ..
        } = classify_response(
            429,
            &headers(&[("Retry-After", "120")]),
            br#"{"error":"Rate limit exceeded. Use the attached challenge tokens...","challenge_tokens":["abc123"],"account":"+15550001111"}"#,
            &settings(),
        ) else {
            panic!("a rate limit is the provider saying not now, not a broken channel");
        };
        assert_eq!(status, Some(429));
        assert_eq!(provider_status, "http_429");
        assert_eq!(retry_after_seconds, Some(120));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("abc123"));
        assert!(warnings[0].contains("rate-limit-challenge"));
    }

    #[test]
    fn a_server_error_stays_on_the_delivery_lane() {
        let Outcome::Rejected {
            status,
            provider_status,
            ..
        } = classify(503, "upstream exploded")
        else {
            panic!("a 5xx is the provider saying not now");
        };
        assert_eq!(status, Some(503));
        assert_eq!(provider_status, "http_503");
    }

    #[test]
    fn a_refused_egress_tells_the_operator_to_set_server_url() {
        for strict in [true, false] {
            let error = misconfiguration(transport_failure(
                "HTTP request to http://signal.test:8080/v2/send is not allowed",
                &settings(),
                strict,
            ));
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(error.public_message.contains("server_url"));
        }
    }

    #[test]
    fn an_unreachable_server_is_upstream_unavailable_on_a_test() {
        let error = misconfiguration(transport_failure("connection refused", &settings(), true));
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
        assert!(error.public_message.contains("http://signal.test:8080"));
    }

    #[test]
    fn an_unreachable_server_is_a_delivery_failure_on_a_live_send() {
        let Outcome::Rejected {
            status,
            provider_status,
            detail,
            ..
        } = transport_failure("connection refused", &settings(), false)
        else {
            panic!("a live send must not report a network blink as a broken setting");
        };
        assert_eq!(status, None);
        assert_eq!(provider_status, "request_failed");
        assert!(detail.contains("http://signal.test:8080"));
    }

    #[test]
    fn quoted_upstream_text_is_bounded() {
        let error = misconfiguration(classify(
            400,
            &format!(r#"{{"error":"{}"}}"#, "e".repeat(1000)),
        ));
        assert!(
            error.public_message.chars().count() < 500,
            "a proxy's wall of text must not become the operator's error"
        );
    }

    // -----------------------------------------------------------------
    // Probe
    // -----------------------------------------------------------------

    #[test]
    fn about_reports_a_server_that_is_not_signal_cli_rest_api() {
        let warnings = about_warnings(b"<html>nope</html>");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("signal-cli-rest-api"));
    }

    #[test]
    fn about_warns_when_v2_is_missing_and_when_the_mode_is_slow() {
        let warnings = about_warnings(
            br#"{"versions":["v1"],"build":2,"mode":"normal","version":"0.100","capabilities":{"v2/send":["quotes","mentions"]}}"#,
        );
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("v2"));
        assert!(warnings[1].contains("json-rpc"));
    }

    #[test]
    fn a_current_server_probes_clean() {
        let warnings = about_warnings(
            br#"{"versions":["v1","v2"],"build":2,"mode":"json-rpc","version":"0.100","capabilities":{"v2/send":["quotes","mentions"]}}"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    // -----------------------------------------------------------------
    // Recipient matching
    // -----------------------------------------------------------------

    #[test]
    fn a_recipient_error_is_matched_to_the_recipient_that_was_sent() {
        let recipients = [
            number("+15550002222"),
            Recipient {
                value: "u:example.01".to_string(),
                kind: RecipientKind::Username,
            },
        ];
        let errors = vec![
            ("+15550002222".to_string(), "Unregistered user".to_string()),
            ("example.01".to_string(), "Untrusted identity".to_string()),
        ];

        assert_eq!(
            recipient_error_for(&errors, &recipients[0]).map(String::as_str),
            Some("Unregistered user")
        );
        assert_eq!(
            recipient_error_for(&errors, &recipients[1]).map(String::as_str),
            Some("Untrusted identity"),
            "the server strips the u: prefix it added"
        );
        assert!(recipient_error_for(&[], &recipients[0]).is_none());
    }

    #[test]
    fn size_formatting_matches_sonarrs_rounding() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(1_536), "1.5 KB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
    }
}
