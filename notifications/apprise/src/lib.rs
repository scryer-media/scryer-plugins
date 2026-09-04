//! Apprise API notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Apprise notification (`src/NzbDrone.Core/Notifications/Apprise/`) is
//! a thin `POST` to a self-hosted Apprise API server: `/notify/{key}` for a
//! stored configuration or `/notify` with a `urls` list for the stateless mode,
//! carrying `title`, `body`, `type`, an optional `tag`, an optional
//! `attachment`, and optional HTTP Basic credentials for whatever fronts the
//! server (`AppriseProxy.cs:30-87`). Its settings validator carries the rules
//! that matter — exactly one of key/urls, a key charset, and "stateless URLs do
//! not support tags" (`AppriseSettings.cs:10-37`).
//!
//! Three things it gets wrong, and one it cannot express:
//!
//! * **A missing configuration key reads as success.** `apprise-api` answers
//!   `204 No Content` with `{"error": "There was no configuration found"}` when
//!   `/notify/{key}` names a key it has never stored (`api/views.py`,
//!   `NotifyView.post`). `204` is a 2xx, so Sonarr's `HttpClient` does not
//!   throw, `SendNotification` returns, and both the send *and the connection
//!   test* report success while nothing was ever delivered. The stateless mode
//!   has the same hole: no usable URL is also a `204`.
//! * **A partial delivery reads as total failure with no detail.** `424` means
//!   "at least one notification could not be sent"; Sonarr collapses it into
//!   `AppriseException("Unable to send Apprise notifications: …")`.
//! * **The key charset rule is narrower than the server's.** Sonarr's
//!   `Matches("^[a-z0-9-]*$")` (`AppriseSettings.cs:20-21`) rejects keys the API
//!   itself routes: `api/urls.py` matches `(?P<key>[\w_-]{1,128})`, i.e.
//!   letters in either case, digits, `_` and `-`, 1–128 characters.
//! * Sonarr sends one operator-chosen `type` for every event, so a failed
//!   download is as neutral as a rename, and it has nowhere to put the facts
//!   Scryer's contract carries (episode, quality, indexer, size, paths).
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * every configuration problem is a typed `PluginError` naming the field —
//!   `server_url`, `configuration_key`, `stateless_urls`, `tags`,
//!   `notification_type`, `auth_username` — instead of the June port's
//!   `error_response`, which told the operator "a notification failed to send"
//!   when the truth was "you set both a key and stateless URLs";
//! * the API's own status table is mapped lane by lane, including the `204` and
//!   `424` Sonarr cannot see, and the server's `{"error", "details"}` body is
//!   parsed so its own words reach the operator;
//! * `notification_type` gains an `auto` option that derives Apprise's type from
//!   the event's severity, so failures arrive as `failure`;
//! * the body is enriched per event from the structured blocks the contract
//!   carries, rather than being `summary_message` alone;
//! * a connection test probes `GET /status` and, when tags are configured,
//!   `GET /json/urls/{key}` — warnings only — so the operator hears about
//!   `APPRISE_STATEFUL_MODE=disabled`, disabled attachments, and tags that match
//!   nothing on the server *before* a real event goes missing.
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper treats every 2xx as success and collapses every non-2xx
//! into `error_response("HTTP N: body", "http_N")`. Apprise's `204` is a 2xx
//! that delivered nothing, its `424` is a delivery failure rather than a
//! configuration one, and its `401`, `404` and `400` are three different
//! settings. None of that survives the shared helper.
//!
//! # Upstream reference
//!
//! Read 2026-09-02, against `caronc/apprise-api` at `master` (v1.5.3, released
//! 2026-08-31) and `caronc/apprise` v1.13.1 (2026-08-31):
//!
//! * <https://github.com/caronc/apprise-api> (README) — `POST /notify/{KEY}`
//!   and stateless `POST /notify`; fields `body` (required), `title`, `type`,
//!   `format`, `tag`, `urls` (stateless only), and `attach`/`attachment`, which
//!   accept a remote `http(s)` URL as well as an uploaded file; JSON, form and
//!   multipart bodies are all accepted; `?tag=`, `?type=`, `?format=` and
//!   `?title=` query parameters are read only when the payload does not carry
//!   the field.
//! * `apprise_api/api/views.py` (`NotifyView`, `StatelessNotifyView`,
//!   `HealthCheckView`, `JsonUrlView`) — the status table this module maps:
//!   `200` sent, `204` no configuration found / "There was no valid URLs
//!   provided to notify", `400` invalid request, `406` recursion limit,
//!   `424` "at least one notification could not be sent", `431` payload too
//!   large, `500` server-side I/O. The response body is
//!   `{"error": <string|null>, "details": <string|array>}` and is JSON **only**
//!   when the request asks for it (`is_json_response`: `Accept` matching
//!   `(text|application)/(x-)?json`, falling back to `Content-Type`).
//! * `apprise_api/api/urls.py` — the `{KEY}` route regex `[\w_-]{1,128}`.
//! * `apprise_api/api/forms.py` — `NotifyForm`/`NotifyByUrlForm`; `type` is one
//!   of `info`/`success`/`warning`/`failure` and `format` one of
//!   `text`/`markdown`/`html`, matching `apprise.NOTIFY_TYPES` and
//!   `apprise.NOTIFY_FORMATS`.
//! * `HealthCheckView` JSON — `{config_lock, attach_lock, stateful_enabled,
//!   max_attachments, attach_size, status}`, `200` healthy / `417` not. It
//!   reports no version, which is why nothing here is version-gated on a
//!   number.
//! * `JsonUrlView` JSON — `{tags: [...], urls: [{tags: [...], ...}]}`, `200`
//!   with a stored configuration, `204` when it is empty.
//! * Environment knobs that change what this channel can do:
//!   `APPRISE_STATEFUL_MODE` (`hash`/`simple`/`disabled` — `disabled` makes
//!   every `/notify/{key}` a dead end), `APPRISE_ATTACH_SIZE` (`0` disables
//!   attachments), `APPRISE_STATELESS_URLS` (the server's own default URL list).
//!   All three are visible through `GET /status` and are surfaced as test
//!   warnings.
//!
//! Apprise is self-hosted and every endpoint used here has existed for the life
//! of the project, so nothing in this channel is version-gated; the two probes
//! degrade to silence on anything they do not recognise.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationSeverity,
    PluginNotificationEpisode, PluginNotificationTargetResult, current_sdk_constraint,
};
use serde_json::{Value, json};

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

const PROVIDER_TYPE: &str = "apprise";
const USER_AGENT: &str = concat!("scryer-apprise-plugin/", env!("CARGO_PKG_VERSION"));

/// The image a connection test attaches when `include_poster` is on and the
/// request carries no poster, standing in for Sonarr's
/// `https://raw.githubusercontent.com/Sonarr/Sonarr/develop/Logo/128.png`
/// (`AppriseProxy.cs:94`). Sonarr attaches its logo so the operator can see that
/// the attachment path works at all — the Apprise server fetches this URL
/// itself, so a broken one turns a passing test into a `424`.
///
/// This is a tracked asset in the Scryer repository, verified reachable
/// 2026-09-02. The `icons/icon-512.png` path the Discord port uses does **not**
/// exist (`apps/scryer-web/public/icons/` is empty in both
/// `scryer-release-next` and `release-0.19.8`, and the raw URL answers `404`).
const SCRYER_LOGO: &str = "https://raw.githubusercontent.com/scryer-media/scryer/main/apps/scryer-web/public/scryer-lockup-light.webp";

/// The one line of upstream text quoted back to the operator, bounded: it ends
/// up in `public_message`, and a reverse proxy answering with an HTML page must
/// not turn a notification failure into a wall of markup.
const MAX_QUOTED_ERROR: usize = 300;

/// `api/urls.py`: `(?P<key>[\w_-]{1,128})`.
const MAX_CONFIGURATION_KEY: usize = 128;

/// Derive Apprise's `type` from the event instead of sending one fixed value.
const NOTIFICATION_TYPE_AUTO: &str = "auto";

/// `AppriseNotificationType.cs`, plus the derived option Sonarr has no way to
/// express. The stored values are unchanged and `info` remains the default, so
/// an existing channel keeps behaving exactly as it did.
const NOTIFICATION_TYPE_OPTIONS: &[(&str, &str)] = &[
    ("info", "Info"),
    ("success", "Success"),
    ("warning", "Warning"),
    ("failure", "Failure"),
    (
        NOTIFICATION_TYPE_AUTO,
        "Automatic (from the event's severity)",
    ),
];

/// `apprise.NOTIFY_FORMATS`. This channel builds plain text and says so rather
/// than relying on the server's default staying `text`; Apprise converts to
/// whatever each destination service wants.
const BODY_FORMAT: &str = "text";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Apprise".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // Apprise is self-hosted: there is no vendor endpoint to prefill and
            // no host set to allowlist. The operator's `server_url` is the only
            // origin this channel ever reaches, and the loader allowlists its
            // host because the value parses as a URL.
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                // The body is plain text; Apprise renders it for each
                // destination service. `format` is sent explicitly as `text`.
                supports_rich_text: false,
                // The poster travels as a URL in `attachment`; the Apprise
                // server fetches it. No bytes are uploaded from here.
                supports_images: true,
                supports_test: true,
                // One `POST` carries one notification to every URL behind the
                // key or in the stateless list — that is fan-out, not batching
                // of distinct notifications.
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![
                    NotificationDeliveryMode::Push,
                    NotificationDeliveryMode::Aggregator,
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
            true,
            None,
            Some("Apprise API base URL, for example http://apprise.example:8000."),
        ),
        field(
            "configuration_key",
            "Configuration Key",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "A configuration stored on the Apprise API server; sends POST /notify/<key>. Letters, digits, underscores and hyphens, up to 128 characters. Mutually exclusive with Stateless URLs.",
            ),
        ),
        field(
            "stateless_urls",
            "Stateless URLs",
            ConfigFieldType::Multiline,
            false,
            None,
            Some(
                "Apprise destination URLs sent with each notification; sends POST /notify. One per line or comma separated. Mutually exclusive with Configuration Key.",
            ),
        ),
        select_field(
            "notification_type",
            "Notification Type",
            Some("info"),
            NOTIFICATION_TYPE_OPTIONS,
        ),
        // Sonarr's field is a `Tag` (`AppriseSettings.cs:59-60`); the June port made
        // it a plain `String`. The stored value is the same comma-separated text
        // either way — Scryer's notification settings UI renders a `Tag` field as
        // a comma-separated text input — so this is a descriptor correction, not
        // a migration.
        tag_field(
            "tags",
            "Tags",
            Some(
                "Apprise tags selecting which of the configuration's URLs to notify. A space means AND, a comma means OR. Only usable with a Configuration Key.",
            ),
        ),
        field(
            "include_poster",
            "Include Poster",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Sends the title's poster URL as an Apprise attachment. The Apprise server downloads it, so it must be reachable from there.",
            ),
        ),
        field(
            "auth_username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "HTTP Basic username, for a reverse proxy in front of the Apprise API. Apprise itself has no authentication.",
            ),
        ),
        field(
            "auth_password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            Some("HTTP Basic password, for a reverse proxy in front of the Apprise API."),
        ),
    ]
}

/// A multi-value field with no fixed option set.
///
/// Scryer's notification settings UI renders a `Tag` field as a plain
/// comma-separated text input, so the stored value is byte-identical to what the
/// `String` field held. Apprise tags are operator-defined, so there is no option
/// list to offer.
fn tag_field(key: &str, label: &str, help_text: Option<&str>) -> ConfigFieldDef {
    field(key, label, ConfigFieldType::Tag, false, None, help_text)
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Which of the API's two notification endpoints this channel uses.
///
/// `AppriseProxy.cs:46-57`: a configuration key wins, then stateless URLs. Here
/// the two are exclusive by validation rather than by precedence, which is what
/// Sonarr's own validator says (`AppriseSettings.cs:16-29`) but its proxy does
/// not enforce.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// `POST /notify/{key}`.
    Stateful { key: String },
    /// `POST /notify` with a `urls` payload field.
    Stateless { urls: String },
}

impl Route {
    fn path(&self) -> String {
        match self {
            Route::Stateful { key } => format!("/notify/{key}"),
            Route::Stateless { .. } => "/notify".to_string(),
        }
    }

    /// The `target` recorded in `target_results`. Stateless URLs carry
    /// credentials (`mailto://user:pass@…`), so only the schemes are reported.
    fn target(&self) -> String {
        match self {
            Route::Stateful { key } => format!("notify/{key}"),
            Route::Stateless { urls } => {
                let schemes = stateless_url_list(urls)
                    .iter()
                    .map(|url| match url.split_once("://") {
                        Some((scheme, _)) => format!("{scheme}://"),
                        None => "?".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if schemes.is_empty() {
                    "notify (stateless)".to_string()
                } else {
                    format!("notify (stateless: {schemes})")
                }
            }
        }
    }
}

/// Everything the renderer and the sender need from configuration, resolved and
/// validated once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    server: String,
    route: Route,
    /// `None` means "derive the type from the event" (`auto`).
    notification_type: Option<String>,
    tags: Vec<String>,
    include_poster: bool,
    auth: Option<(String, String)>,
}

impl Settings {
    /// `strict` is the Test-time posture. Rules whose only consequence is a
    /// silently narrower audience — tags configured alongside stateless URLs,
    /// a stateless entry that is not an Apprise URL — fail the connection test
    /// and degrade to a warning on a live send, because losing a notification is
    /// worse than delivering it slightly wrong. Rules whose consequence is
    /// delivering to the *wrong* audience, or to nobody at all, are errors on
    /// every send.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let server = normalized_server(required_config("server_url").map_err(config_error)?)?;
        let configuration_key = config_value("configuration_key");
        let stateless_urls = config_value("stateless_urls");
        let mut warnings = Vec::new();

        let route = resolve_route(
            configuration_key.as_deref(),
            stateless_urls.as_deref(),
            strict,
            &mut warnings,
        )?;

        let mut tags = validated_tags(&config_csv("tags"))?;
        if !tags.is_empty()
            && let Route::Stateless { .. } = route
        {
            // `AppriseSettings.cs:31-33`: "Stateless URLs do not support tags".
            // The rule survives current documentation for a different reason
            // than Sonarr's: `StatelessNotifyView` does accept a `tag`, but the
            // URLs it builds carry no tags, so anything but the implicit `all`
            // matches nothing and the server answers `204`/`424` having
            // notified no one.
            if strict {
                return Err(plugin_error(
                    PluginErrorCode::InvalidConfig,
                    "tags cannot be used with stateless_urls: a stateless notification has no tagged configuration to match, so a tag filter selects nothing. Store the URLs on the Apprise server and use configuration_key, or clear tags.".to_string(),
                    Some(format!("configured tags: {}", tags.join(", "))),
                ));
            }
            warnings.push(format!(
                "tags ({}) are ignored with stateless_urls: a stateless notification has no tagged configuration to match, so every configured URL is notified",
                tags.join(", ")
            ));
            tags.clear();
        }

        let notification_type = validated_notification_type(
            config_value("notification_type")
                .as_deref()
                .unwrap_or("info"),
        )?;

        let username = config_value("auth_username");
        let password = config_value("auth_password");
        // `AppriseProxy.cs:69-72`: either half is enough to send credentials.
        let auth = (username.is_some() || password.is_some())
            .then(|| (username.unwrap_or_default(), password.unwrap_or_default()));

        Ok((
            Self {
                server,
                route,
                notification_type,
                tags,
                include_poster: config_bool("include_poster"),
                auth,
            },
            warnings,
        ))
    }
}

/// `AppriseSettingsValidator` (`AppriseSettings.cs:16-29`): exactly one of the
/// two routes.
fn resolve_route(
    configuration_key: Option<&str>,
    stateless_urls: Option<&str>,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Route, PluginError> {
    match (configuration_key, stateless_urls) {
        (Some(_), Some(_)) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "set either configuration_key or stateless_urls, not both".to_string(),
            None,
        )),
        (None, None) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "set either configuration_key (a configuration stored on the Apprise server) or stateless_urls (Apprise URLs sent with each notification)".to_string(),
            None,
        )),
        (Some(key), None) => Ok(Route::Stateful {
            key: validated_configuration_key(key)?,
        }),
        (None, Some(urls)) => Ok(Route::Stateless {
            urls: validated_stateless_urls(urls, strict, warnings)?,
        }),
    }
}

/// `AppriseSettings.cs:14`: `RuleFor(c => c.ServerUrl).IsValidUrl()`.
///
/// Sonarr can only say this through its settings form. Here it is a typed
/// `InvalidConfig` naming the field, because the alternative — the June port's
/// bare `trim_end_matches('/')` — turns `apprise.example:8000` into a request
/// the host refuses with a message about an unsupported scheme.
fn normalized_server(raw: String) -> Result<String, PluginError> {
    // `AppriseProxy.cs:42`: `TrimEnd('/', ' ')`.
    let trimmed = raw.trim().trim_end_matches(['/', ' ']);
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "server_url must be an absolute http:// or https:// URL, for example http://apprise.example:8000".to_string(),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    let host_at = lower.find("//").map(|at| at + 2).unwrap_or(0);
    if trimmed.len() <= host_at {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "server_url has no host, for example http://apprise.example:8000".to_string(),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    Ok(trimmed.to_string())
}

/// `api/urls.py`: `^notify/(?P<key>[\w_-]{1,128})/?$`.
///
/// Sonarr's `Matches("^[a-z0-9-]*$")` (`AppriseSettings.cs:20-21`) is narrower than
/// the server's own route and rejects keys the API happily serves — `MyKey`,
/// `home_lab`. The current documentation wins: the charset here is the route's.
fn validated_configuration_key(raw: &str) -> Result<String, PluginError> {
    let key = raw.trim();
    if key.len() > MAX_CONFIGURATION_KEY {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "configuration_key must be at most {MAX_CONFIGURATION_KEY} characters; got {}",
                key.len()
            ),
            None,
        ));
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "configuration_key may only contain letters, digits, underscores and hyphens; got {key:?}"
            ),
            Some("the Apprise API routes /notify/<key> as [\\w_-]{1,128}".to_string()),
        ));
    }
    Ok(key.to_string())
}

/// The stateless URL list, normalised to the comma-separated form Apprise parses
/// and checked for entries that are plainly not Apprise URLs.
///
/// The field is multiline, so the stored value routinely contains newlines.
/// Apprise splits on whitespace as well as commas, but normalising here means
/// the payload is the same shape whatever the operator typed.
fn validated_stateless_urls(
    raw: &str,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<String, PluginError> {
    let entries = stateless_url_list(raw);
    if entries.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "stateless_urls contains no Apprise URLs".to_string(),
            None,
        ));
    }

    let malformed: Vec<&String> = entries
        .iter()
        .filter(|entry| !entry.contains("://"))
        .collect();
    if !malformed.is_empty() {
        let listed = malformed
            .iter()
            .map(|entry| entry.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if strict {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "stateless_urls contains entries that are not Apprise URLs (every Apprise URL has a scheme, for example discord://…): {listed}"
                ),
                None,
            ));
        }
        // A live send keeps going: Apprise drops what it cannot parse and
        // notifies the rest, which beats losing the notification entirely.
        warnings.push(format!(
            "stateless_urls contains entries the Apprise server will not be able to parse: {listed}"
        ));
    }

    Ok(entries.join(","))
}

/// Split a stateless URL field on the separators Apprise itself accepts.
fn stateless_url_list(raw: &str) -> Vec<String> {
    raw.split([',', '\n', '\r', '\t', ' ', ';'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// Apprise's tag expression grammar (`api/views.py`, `parse_tag_expression`):
/// the whole expression matches `^[a-z0-9\s| ,_:+&-]+$` case-insensitively, with
/// a comma or `|` meaning OR and a space, `+` or `&` meaning AND.
///
/// This is an error on every send rather than a Test-time-only rule: an
/// unparseable tag is a `400` from the server, and — worse — a *mistyped* tag
/// that happens to parse selects the wrong audience. Dropping it would fall back
/// to Apprise's implicit `all` and notify every URL behind the key, which is the
/// one outcome an operator using tags is trying to avoid.
fn validated_tags(raw: &[String]) -> Result<Vec<String>, PluginError> {
    let mut tags = Vec::new();
    for value in raw {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || " |_:+&-".contains(ch))
        {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "tags may only contain letters, digits, spaces and the characters | _ : + & -; got {value:?}"
                ),
                Some(
                    "a space, + or & means AND; a comma or | means OR (Apprise tag expressions)"
                        .to_string(),
                ),
            ));
        }
        let value = value.to_string();
        if !tags.contains(&value) {
            tags.push(value);
        }
    }
    Ok(tags)
}

/// `apprise.NOTIFY_TYPES` plus this channel's `auto`. `None` is `auto`.
fn validated_notification_type(raw: &str) -> Result<Option<String>, PluginError> {
    let value = raw.trim().to_ascii_lowercase();
    if value == NOTIFICATION_TYPE_AUTO {
        return Ok(None);
    }
    if NOTIFICATION_TYPE_OPTIONS
        .iter()
        .any(|(key, _)| *key == value)
    {
        return Ok(Some(value));
    }
    Err(plugin_error(
        PluginErrorCode::InvalidConfig,
        format!(
            "notification_type must be one of {}; got {value:?}",
            NOTIFICATION_TYPE_OPTIONS
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None,
    ))
}

// ---------------------------------------------------------------------------
// Notification type
// ---------------------------------------------------------------------------

/// The dispatcher stamps a severity on every notification
/// (`crates/scryer-application/src/notifications/dispatcher.rs:895`); the
/// fallback mirrors its own mapping (`:920-928`) for a host that does not.
fn severity(req: &PluginNotificationRequest) -> NotificationSeverity {
    if let Some(severity) = req.severity {
        return severity;
    }
    match req.event_type {
        NotificationEventType::Download
        | NotificationEventType::ImportRejected
        | NotificationEventType::SubtitleSearchFailed => NotificationSeverity::Error,
        NotificationEventType::HealthIssue => NotificationSeverity::Warning,
        _ => NotificationSeverity::Info,
    }
}

/// Apprise's `type`, which every destination service turns into a colour, an
/// icon or a priority.
///
/// Sonarr sends the configured value for every event (`AppriseProxy.cs:37`), so
/// a failed download looks exactly like a rename. Under `auto` the severity
/// decides first — that is the field the core actually fills — and the event
/// type only distinguishes `success` from `info` among the informational ones.
fn notification_type(req: &PluginNotificationRequest, settings: &Settings) -> &'static str {
    if let Some(configured) = settings.notification_type.as_deref() {
        return match configured {
            "success" => "success",
            "warning" => "warning",
            "failure" => "failure",
            _ => "info",
        };
    }
    match severity(req) {
        NotificationSeverity::Error => "failure",
        NotificationSeverity::Warning => "warning",
        NotificationSeverity::Info => match req.event_type {
            NotificationEventType::ImportComplete
            | NotificationEventType::Upgrade
            | NotificationEventType::PostProcessingCompleted
            | NotificationEventType::TitleAdded
            | NotificationEventType::SubtitleDownloaded
            | NotificationEventType::MediaRequestApproved
            | NotificationEventType::HealthRestored => "success",
            // The one event whose whole purpose is that someone must act on it.
            // The dispatcher gives it `Info` because it has no emitter yet.
            NotificationEventType::ManualInteractionRequired => "warning",
            _ => "info",
        },
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// `Apprise.cs:23-71` sends a fixed constant per event ("Episode Grabbed",
/// "Import Complete", …). Scryer's dispatcher already composes an
/// event-specific, title-bearing heading in `summary_title` ("Grabbed: Example
/// Show"), which is strictly more informative wherever Apprise puts a title.
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

/// `body`, which Apprise requires. Sonarr hands the proxy one prose sentence;
/// Scryer's contract carries the facts separately, so each present block adds a
/// line. The sparse shape the core sends today renders exactly the one line the
/// June port sent.
fn body(req: &PluginNotificationRequest) -> String {
    let mut lines: Vec<String> = Vec::new();
    let summary = req.summary_message.trim();
    if !summary.is_empty() {
        lines.push(summary.to_string());
    }
    for (label, value) in detail_lines(req) {
        lines.push(format!("{label}: {value}"));
    }
    if lines.is_empty() {
        lines.push(heading(req));
    }
    lines.join("\n")
}

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
        // (`crates/scryer-application/src/notifications/dispatcher.rs:34,418-448`,
        // release-0.19.8). A successful import is `ImportComplete`/`Upgrade`, so
        // this arm renders a failure and never an import path.
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

/// `AppriseProxy.cs:32-67` on Scryer's contract: `title`, `body`, `type`, plus
/// `urls` in the stateless mode, `tag` when configured, `attachment` when the
/// poster is wanted, and an explicit `format`.
fn build_payload(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Value {
    let mut payload = json!({
        "title": heading(req),
        "body": body(req),
        "type": notification_type(req, settings),
        "format": BODY_FORMAT,
    });

    if let Route::Stateless { urls } = &settings.route {
        payload["urls"] = Value::String(urls.clone());
    }

    if !settings.tags.is_empty() {
        // `AppriseProxy.cs:59-62`: `settings.Tags.Join(",")`. A comma is
        // Apprise's OR; an operator wanting AND writes the space inside one tag
        // value, which `config_csv` preserves.
        payload["tag"] = Value::String(settings.tags.join(","));
    }

    if let Some(attachment) = attachment(req, settings, warnings) {
        // `attach`, `attachment` and `attachments` are all accepted; this is the
        // key Sonarr sends (`ApprisePayload.Attachment`) and the one with the
        // longest history in the API.
        payload["attachment"] = Value::String(attachment);
    }

    payload
}

/// `AppriseProxy.cs:64-67` plus Sonarr's test logo (`:94`).
///
/// The Apprise **server** downloads this URL, so a relative path or a
/// non-http(s) value is not merely useless, it makes the server answer `424`.
/// It is dropped with a warning instead.
fn attachment(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if !settings.include_poster {
        return None;
    }
    match poster_url(req) {
        Some(poster) if is_absolute_http(&poster) => Some(poster),
        Some(poster) => {
            warnings.push(format!(
                "the title's poster is not an absolute http(s) URL and was not attached: {poster}"
            ));
            None
        }
        // Sonarr attaches its own logo to the test so the operator can see the
        // attachment path work end to end. Only on a test: a real event with no
        // poster should not carry a stock image.
        None if req.is_test => Some(SCRYER_LOGO.to_string()),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    let payload = build_payload(req, &settings, &mut warnings);
    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            return PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                "could not encode the Apprise notification payload".to_string(),
                Some(error.to_string()),
            ));
        }
    };

    if req.is_test {
        warnings.extend(probe_server(&settings));
        warnings.extend(probe_tags(&settings));
    }

    let url = format!("{}{}", settings.server, settings.route.path());
    let mut request = HttpRequest::new(&url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        // Without this the API answers `text/plain`/`text/html` and its
        // `{"error", "details"}` body — the only place it says *why* — never
        // reaches the operator (`api/utils.py::is_json_response`).
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    // `X-Apprise-ID` is what the server correlates its own logs on and what its
    // recursion guard counts against, so giving it the event's own id makes a
    // notification traceable from Scryer's log to the Apprise server's.
    if let Some(id) = req
        .event_id
        .as_deref()
        .or(req.correlation_id.as_deref())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        request = request.with_header("X-Apprise-ID", id);
    }
    if let Some((username, password)) = &settings.auth {
        request = request.with_header("Authorization", basic_auth_header(username, password));
    }

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            &settings.route,
            warnings,
        ),
        Err(error) => {
            // The host answers a refused or failed egress in-band; that is the
            // server being unreachable, not a misconfigured channel.
            let mut failure = error_response(format!("request failed: {error}"), None);
            failure.warnings = warnings;
            failure.target_results = vec![PluginNotificationTargetResult {
                target: settings.route.target(),
                success: false,
                status: None,
                error: Some(format!("request failed: {error}")),
            }];
            PluginResult::Ok(failure)
        }
    }
}

/// The API's `{"error": <string|null>, "details": <string|array>}` response.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct AppriseBody {
    /// `AppriseError.Error` (`AppriseError.cs`).
    error: Option<String>,
    /// `details` reduced to the lines that say something went wrong. In a JSON
    /// response each entry is `["LEVEL", "timestamp", "message"]`.
    problems: Vec<String>,
    /// Whether the body parsed as a JSON object at all. A `false` here is the
    /// single most useful signal this channel has: with `Accept:
    /// application/json` the API answers JSON on every documented status, so
    /// anything else means something that is not the Apprise API answered.
    is_json: bool,
    raw: String,
}

impl AppriseBody {
    /// The one line of upstream text quoted back to the operator, bounded.
    fn detail(&self, status: u16) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(error) = self
            .error
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            parts.push(error.to_string());
        }
        if !self.problems.is_empty() {
            parts.push(self.problems.join("; "));
        }
        if parts.is_empty() {
            return match self.raw.trim() {
                "" => format!("HTTP {status}"),
                raw => ellipsize(raw, MAX_QUOTED_ERROR),
            };
        }
        ellipsize(&parts.join(" — "), MAX_QUOTED_ERROR)
    }
}

fn parse_apprise_body(body: &[u8]) -> AppriseBody {
    let raw = String::from_utf8_lossy(body).to_string();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return AppriseBody {
            raw,
            ..AppriseBody::default()
        };
    };
    AppriseBody {
        error: map
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|error| !error.is_empty())
            .map(str::to_string),
        problems: parse_details(map.get("details")),
        is_json: true,
        raw,
    }
}

/// `details` is a list of log records the API captured while notifying, each
/// `["LEVEL", "<timestamp>", "<message>"]`. Only the ones that report a problem
/// are quoted: on a `424` those name the service that refused, which is the
/// detail Sonarr's single exception string throws away.
fn parse_details(details: Option<&Value>) -> Vec<String> {
    match details {
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![text.to_string()]
            }
        }
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(|entry| {
                let record = entry.as_array()?;
                let level = record.first()?.as_str()?.trim().to_ascii_uppercase();
                if !matches!(level.as_str(), "ERROR" | "CRITICAL" | "WARNING") {
                    return None;
                }
                let message = record.last()?.as_str()?.trim();
                (!message.is_empty()).then(|| message.to_string())
            })
            .collect(),
        _ => Vec::new(),
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

/// Sonarr sees two outcomes: the `HttpClient` did not throw (success, including
/// the `204` that delivered nothing) or it did (`AppriseException`, with a 401
/// branch and an "error body" branch reachable only from `Test`,
/// `AppriseProxy.cs:100-125`). Scryer's typed error lane exists on every send,
/// so the operator is always told which setting to fix.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    route: &Route,
    mut warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let answer = parse_apprise_body(body);
    let detail = answer.detail(status);
    let debug = format!("HTTP {status}: {detail}");

    // `204 No Content` is a 2xx that notified nobody: either `/notify/{key}`
    // names a configuration the server has never stored, or the stateless URL
    // list produced nothing Apprise could use ("There was no configuration
    // found" / "There was no valid URLs provided to notify", `api/views.py`).
    // Sonarr reports this as a successful send *and* a passing connection test.
    if status == 204 {
        return PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            match route {
                Route::Stateful { key } => format!(
                    "the Apprise server has no configuration stored under configuration_key '{key}', so nothing was notified: {detail}. Add it with POST /add/{key}, or switch to stateless_urls."
                ),
                Route::Stateless { .. } => format!(
                    "the Apprise server found no usable URL in stateless_urls, so nothing was notified: {detail}"
                ),
            },
            Some(debug),
        ));
    }

    if (200..300).contains(&status) {
        let mut response = ok_response();
        if !answer.is_json {
            // Accepted, but not by something that answered like the Apprise API.
            // A warning rather than a failure: the message may well have been
            // delivered, and refusing a working channel over a proxy's response
            // body would be worse than saying so.
            warnings.push(format!(
                "the server accepted the notification with HTTP {status} but did not answer like an Apprise API server; check that server_url points at apprise-api"
            ));
        }
        // Apprise logs warnings for services it skipped even on a full success.
        warnings.extend(answer.problems.iter().cloned());
        response.warnings = warnings;
        response.target_results = vec![PluginNotificationTargetResult {
            target: route.target(),
            success: true,
            status: Some(format!("http_{status}")),
            error: None,
        }];
        return PluginResult::Ok(response);
    }

    // A non-2xx that is not the API's documented JSON did not come from the
    // Apprise API: an authenticating reverse proxy, a captive portal, or an
    // unrelated service on that origin. Naming `auth_username` there would send
    // the operator to the wrong setting.
    if !answer.is_json && !(500..600).contains(&status) && status != 429 && status != 401 {
        return PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "server_url did not answer like an Apprise API server (HTTP {status}): {detail}. Check the URL and anything proxying it."
            ),
            Some(debug),
        ));
    }

    match status {
        // Apprise itself has no authentication, so credentials only ever belong
        // to whatever fronts it. `AppriseProxy.cs:102-106` names `AuthUsername`
        // for a 401; 403 is the same pair from the other side.
        401 | 403 => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!(
                "the HTTP Basic credentials were rejected (HTTP {status}): {detail}. The Apprise API has no authentication of its own, so auth_username and auth_password belong to the reverse proxy in front of it."
            ),
            Some(debug),
        )),
        // Every version of the API serves `/notify` and `/notify/{key}`, so a
        // 404 means the base URL is wrong or points somewhere else.
        404 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "server_url does not expose the Apprise API's {} endpoint (HTTP 404): {detail}. Check the URL, including any path prefix.",
                route.path()
            ),
            Some(debug),
        )),
        // "At least one notification could not be sent" — the destination
        // services behind the key or the URL list refused, which is a delivery
        // outcome and not something the operator can fix in this channel's
        // settings. It may be partial: the API returns one status for the whole
        // fan-out.
        424 => {
            let mut failure = error_response(
                format!(
                    "the Apprise server could not deliver to at least one of its targets: {detail}"
                ),
                Some("http_424".to_string()),
            );
            failure.warnings = warnings;
            failure.target_results = vec![PluginNotificationTargetResult {
                target: route.target(),
                success: false,
                status: Some("http_424".to_string()),
                error: Some(detail),
            }];
            PluginResult::Ok(failure)
        }
        // The request this plugin built is wrong — an unusable `type`,
        // `format`, tag expression or attachment. The operator has nothing to
        // fix beyond what validation already covers, so it is reported as this
        // plugin's fault.
        400 | 405 | 406 | 431 => PluginResult::Err(plugin_error(
            PluginErrorCode::Permanent,
            format!(
                "the Apprise server rejected the request this plugin built (HTTP {status}): {detail}"
            ),
            Some(debug),
        )),
        // Apprise does not rate-limit, but a reverse proxy in front of it does,
        // and `Retry-After` is the one thing the core can act on.
        // Everything else — including the `500` the API returns when it cannot
        // read or write its own configuration store — is the provider saying
        // "not now": the delivery lane, not the configuration lane.
        _ => {
            let mut failure = error_response(
                format!("HTTP {status}: {detail}"),
                Some(format!("http_{status}")),
            );
            failure.retry_after_seconds = header(headers, "retry-after")
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|seconds| *seconds >= 0)
                .map(|seconds| seconds.max(1));
            failure.warnings = warnings;
            failure.target_results = vec![PluginNotificationTargetResult {
                target: route.target(),
                success: false,
                status: Some(format!("http_{status}")),
                error: Some(detail),
            }];
            PluginResult::Ok(failure)
        }
    }
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

// ---------------------------------------------------------------------------
// Test-time probes
//
// Sonarr's `Test` is the send itself (`AppriseProxy.cs:90-99`), so it can only
// discover what a notification discovers. Two unauthenticated GETs cost one
// round trip each and answer questions the send cannot: is this an Apprise API
// server at all, is stateful storage switched off (which makes every
// `/notify/{key}` a dead end), are attachments disabled, and do the configured
// tags match anything the server knows about?
//
// Everything they find is a warning. A probe that cannot decide must never stop
// a delivery, and the `POST` immediately afterwards produces the real error when
// the server is genuinely wrong.
// ---------------------------------------------------------------------------

fn probe_server(settings: &Settings) -> Vec<String> {
    let request = HttpRequest::new(format!("{}/status", settings.server))
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    let Ok(response) = http::request::<Vec<u8>>(&request, None) else {
        return Vec::new();
    };
    status_warnings(response.status_code(), &response.body(), settings)
}

/// `HealthCheckView`: `{config_lock, attach_lock, stateful_enabled,
/// max_attachments, attach_size, status}`, `200` healthy and `417` not.
fn status_warnings(status: u16, body: &[u8], settings: &Settings) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return vec![format!(
            "GET {}/status did not answer with Apprise API health information (HTTP {status}): check that server_url points at apprise-api",
            settings.server
        )];
    };

    let health = map
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|health| !health.is_empty());
    if status != 200 {
        warnings.push(format!(
            "the Apprise server reports itself unhealthy (HTTP {status}{}); notifications may fail",
            health.map(|h| format!(": {h}")).unwrap_or_default()
        ));
    }

    // `APPRISE_STATEFUL_MODE=disabled`. Every `/notify/{key}` on such a server
    // answers `204`, so a stateful channel can never deliver.
    if matches!(settings.route, Route::Stateful { .. })
        && map.get("stateful_enabled").and_then(Value::as_bool) == Some(false)
    {
        warnings.push(
            "this Apprise server has stateful configuration storage disabled (APPRISE_STATEFUL_MODE=disabled), so a configuration_key can never resolve; use stateless_urls instead".to_string(),
        );
    }

    if settings.include_poster {
        let attach_size = map.get("attach_size").and_then(Value::as_i64);
        let attach_lock = map.get("attach_lock").and_then(Value::as_bool);
        let max_attachments = map.get("max_attachments").and_then(Value::as_i64);
        if attach_lock == Some(true) || attach_size == Some(0) || max_attachments == Some(0) {
            warnings.push(
                "include_poster is on but this Apprise server has attachments disabled (APPRISE_ATTACH_SIZE=0 or an attachment lock), so the poster will be refused".to_string(),
            );
        }
    }

    warnings
}

/// `JsonUrlView`: the tags the stored configuration actually defines.
///
/// Only worth a round trip when tags are configured, and only in the stateful
/// mode — a tag that matches nothing is the failure mode where the channel looks
/// healthy and silently notifies no one.
fn probe_tags(settings: &Settings) -> Vec<String> {
    let Route::Stateful { key } = &settings.route else {
        return Vec::new();
    };
    if settings.tags.is_empty() {
        return Vec::new();
    }
    let request = HttpRequest::new(format!("{}/json/urls/{key}", settings.server))
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    let Ok(response) = http::request::<Vec<u8>>(&request, None) else {
        return Vec::new();
    };
    tag_warnings(response.status_code(), &response.body(), &settings.tags)
}

fn tag_warnings(status: u16, body: &[u8], tags: &[String]) -> Vec<String> {
    if status == 204 {
        return vec![
            "the configuration stored under configuration_key is empty, so this channel has no destination URLs".to_string(),
        ];
    }
    if status != 200 {
        return Vec::new();
    }
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let Some(known) = map.get("tags").and_then(Value::as_array) else {
        return Vec::new();
    };
    let known: Vec<String> = known
        .iter()
        .filter_map(Value::as_str)
        .map(|tag| tag.trim().to_ascii_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    if known.is_empty() {
        return Vec::new();
    }

    let mut warnings = Vec::new();
    for expression in tags {
        // A tag value may be an AND group ("tag1 tag2") and may carry Apprise's
        // numeric weight prefixes/suffixes ("3:tag1"). Every atom of an AND
        // group has to exist for the group to select anything.
        for atom in expression.split([' ', '+', '&']) {
            let atom = atom.trim().trim_matches(':');
            let atom = atom
                .split(':')
                .find(|part| !part.is_empty() && !part.chars().all(|ch| ch.is_ascii_digit()))
                .unwrap_or(atom)
                .to_ascii_lowercase();
            if atom.is_empty() || atom == "all" {
                continue;
            }
            if !known.contains(&atom) {
                warnings.push(format!(
                    "no URL in the stored Apprise configuration carries the tag '{atom}'; the server knows: {}",
                    known.join(", ")
                ));
            }
        }
    }
    warnings.dedup();
    warnings
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
        NotificationMediaUpdateType, PluginNotificationApp, PluginNotificationApplicationUpdate,
        PluginNotificationDownload, PluginNotificationExternalIds, PluginNotificationFile,
        PluginNotificationHealth, PluginNotificationImport, PluginNotificationManualInteraction,
        PluginNotificationMediaFile, PluginNotificationMediaUpdate, PluginNotificationRelease,
        PluginNotificationTitle,
    };

    fn stateful_settings() -> Settings {
        Settings {
            server: "https://apprise.test".to_string(),
            route: Route::Stateful {
                key: "scryer".to_string(),
            },
            notification_type: Some("info".to_string()),
            tags: Vec::new(),
            include_poster: false,
            auth: None,
        }
    }

    fn stateless_settings() -> Settings {
        Settings {
            route: Route::Stateless {
                urls: "discord://webhook/token,mailto://user:pass@example.test".to_string(),
            },
            ..stateful_settings()
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
            summary_message: "Example Show - 1x01".to_string(),
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
            file: None,
            media_files: Vec::new(),
            health: None,
            application_update: None,
            manual_interaction: None,
            media_request: None,
        }
    }

    /// Everything the contract can carry, so the renderer is exercised against
    /// the shape the core will eventually send rather than the one it sends now.
    fn populated_request() -> PluginNotificationRequest {
        PluginNotificationRequest {
            title: Some(PluginNotificationTitle {
                id: Some("title-1".to_string()),
                name: "Example Show".to_string(),
                facet: "series".to_string(),
                year: Some(2026),
                slug: Some("example-show".to_string()),
                path: Some("/media/TV/Example Show".to_string()),
                overview: Some("An example.".to_string()),
                sort_title: None,
                background_url: Some("https://images.test/background.jpg".to_string()),
                poster_url: Some("https://images.test/poster.jpg".to_string()),
                tags: Vec::new(),
                aliases: Vec::new(),
                original_language: None,
                original_country: None,
                external_ids: PluginNotificationExternalIds {
                    tvdb_id: Some("12345".to_string()),
                    ..PluginNotificationExternalIds::default()
                },
            }),
            episode: Some(PluginNotificationEpisode {
                display: Some("1x01 - Pilot".to_string()),
                ..PluginNotificationEpisode::default()
            }),
            release: Some(PluginNotificationRelease {
                source_title: Some("Example.Show.S01E01.1080p.WEB-DL-GROUP".to_string()),
                quality: Some("WEBDL-1080p".to_string()),
                release_group: Some("GROUP".to_string()),
                indexer: Some("Example Indexer".to_string()),
                ..PluginNotificationRelease::default()
            }),
            download: Some(PluginNotificationDownload {
                client_name: Some("Weaver".to_string()),
                size_bytes: Some(2_147_483_648),
                ..PluginNotificationDownload::default()
            }),
            ..request(NotificationEventType::Grab)
        }
    }

    // -----------------------------------------------------------------------
    // Payload
    // -----------------------------------------------------------------------

    #[test]
    fn a_stateful_payload_carries_the_sonarr_fields_and_no_urls() {
        let mut warnings = Vec::new();
        let payload = build_payload(&populated_request(), &stateful_settings(), &mut warnings);

        assert_eq!(payload["title"], "Grabbed: Example Show");
        assert_eq!(payload["type"], "info");
        assert_eq!(payload["format"], BODY_FORMAT);
        assert!(
            payload.get("urls").is_none(),
            "a stateful notification must not carry a urls list: {payload}"
        );
        assert!(payload.get("tag").is_none());
        assert!(payload.get("attachment").is_none());
        assert!(warnings.is_empty(), "{warnings:?}");

        let body = payload["body"].as_str().expect("a body");
        for expected in [
            "Example Show - 1x01",
            "Episode: 1x01 - Pilot",
            "Quality: WEBDL-1080p",
            "Release: Example.Show.S01E01.1080p.WEB-DL-GROUP",
            "Release Group: GROUP",
            "Indexer: Example Indexer",
            "Size: 2 GB",
            "Client: Weaver",
        ] {
            assert!(body.contains(expected), "body missing {expected:?}: {body}");
        }
    }

    /// The shape the core actually sends today: a summary and nothing else.
    #[test]
    fn a_sparse_request_renders_the_summary_alone() {
        let mut warnings = Vec::new();
        let payload = build_payload(
            &request(NotificationEventType::Grab),
            &stateful_settings(),
            &mut warnings,
        );
        assert_eq!(payload["body"], "Example Show - 1x01");
        assert_eq!(payload["title"], "Grabbed: Example Show");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn an_event_with_no_summary_still_has_a_body() {
        // Apprise requires `body`; an empty one is a 400.
        let mut req = request(NotificationEventType::Test);
        req.summary_message = "   ".to_string();
        req.summary_title = "Scryer Test Notification".to_string();
        let mut warnings = Vec::new();
        let payload = build_payload(&req, &stateful_settings(), &mut warnings);
        assert_eq!(payload["body"], "Scryer Test Notification");
    }

    #[test]
    fn a_stateless_payload_carries_the_normalised_url_list() {
        let mut warnings = Vec::new();
        let payload = build_payload(&populated_request(), &stateless_settings(), &mut warnings);
        assert_eq!(
            payload["urls"],
            "discord://webhook/token,mailto://user:pass@example.test"
        );
    }

    #[test]
    fn tags_are_joined_with_commas() {
        let settings = Settings {
            tags: vec!["family".to_string(), "phone tablet".to_string()],
            ..stateful_settings()
        };
        let mut warnings = Vec::new();
        let payload = build_payload(&populated_request(), &settings, &mut warnings);
        assert_eq!(payload["tag"], "family,phone tablet");
    }

    // -----------------------------------------------------------------------
    // Attachment (L1)
    // -----------------------------------------------------------------------

    #[test]
    fn the_poster_is_attached_only_when_asked_for() {
        let settings = Settings {
            include_poster: true,
            ..stateful_settings()
        };
        let mut warnings = Vec::new();
        let payload = build_payload(&populated_request(), &settings, &mut warnings);
        assert_eq!(payload["attachment"], "https://images.test/poster.jpg");
        assert!(warnings.is_empty(), "{warnings:?}");

        let mut warnings = Vec::new();
        let payload = build_payload(&populated_request(), &stateful_settings(), &mut warnings);
        assert!(payload.get("attachment").is_none());
    }

    #[test]
    fn a_relative_poster_is_dropped_with_a_warning() {
        let mut req = populated_request();
        req.title.as_mut().expect("a title").poster_url = Some("/media/poster.jpg".to_string());
        req.title.as_mut().expect("a title").background_url = None;
        let settings = Settings {
            include_poster: true,
            ..stateful_settings()
        };
        let mut warnings = Vec::new();
        let payload = build_payload(&req, &settings, &mut warnings);
        assert!(payload.get("attachment").is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("/media/poster.jpg"));
    }

    #[test]
    fn a_test_with_no_poster_attaches_the_scryer_logo() {
        let settings = Settings {
            include_poster: true,
            ..stateful_settings()
        };
        let mut warnings = Vec::new();
        let payload = build_payload(
            &request(NotificationEventType::Test),
            &settings,
            &mut warnings,
        );
        assert_eq!(payload["attachment"], SCRYER_LOGO);

        // A real event with no poster gets no stock image.
        let mut warnings = Vec::new();
        let payload = build_payload(
            &request(NotificationEventType::Grab),
            &settings,
            &mut warnings,
        );
        assert!(payload.get("attachment").is_none());
    }

    // -----------------------------------------------------------------------
    // Notification type (M1)
    // -----------------------------------------------------------------------

    #[test]
    fn a_configured_type_is_sent_for_every_event_the_way_sonarr_does() {
        for configured in ["info", "success", "warning", "failure"] {
            let settings = Settings {
                notification_type: Some(configured.to_string()),
                ..stateful_settings()
            };
            for event in [
                NotificationEventType::Grab,
                NotificationEventType::Download,
                NotificationEventType::ImportComplete,
                NotificationEventType::HealthIssue,
            ] {
                let mut req = request(event);
                req.severity = None;
                assert_eq!(notification_type(&req, &settings), configured);
            }
        }
    }

    #[test]
    fn auto_maps_severity_and_event_onto_apprise_types() {
        let settings = Settings {
            notification_type: None,
            ..stateful_settings()
        };
        let cases: &[(NotificationEventType, Option<NotificationSeverity>, &str)] = &[
            // Severity decides first — that is the field the core fills.
            (
                NotificationEventType::Grab,
                Some(NotificationSeverity::Error),
                "failure",
            ),
            (
                NotificationEventType::Grab,
                Some(NotificationSeverity::Warning),
                "warning",
            ),
            (NotificationEventType::Grab, None, "info"),
            // `Download` is a FAILED download; the dispatcher stamps Error.
            (NotificationEventType::Download, None, "failure"),
            (NotificationEventType::ImportRejected, None, "failure"),
            (NotificationEventType::SubtitleSearchFailed, None, "failure"),
            (NotificationEventType::HealthIssue, None, "warning"),
            (NotificationEventType::HealthRestored, None, "success"),
            (NotificationEventType::ImportComplete, None, "success"),
            (NotificationEventType::Upgrade, None, "success"),
            (
                NotificationEventType::PostProcessingCompleted,
                None,
                "success",
            ),
            (NotificationEventType::TitleAdded, None, "success"),
            (NotificationEventType::SubtitleDownloaded, None, "success"),
            (NotificationEventType::MediaRequestApproved, None, "success"),
            (
                NotificationEventType::ManualInteractionRequired,
                None,
                "warning",
            ),
            (NotificationEventType::Rename, None, "info"),
            (NotificationEventType::TitleDeleted, None, "info"),
            (NotificationEventType::FileDeleted, None, "info"),
            (NotificationEventType::Test, None, "info"),
        ];
        for (event, severity, expected) in cases {
            let mut req = request(*event);
            req.severity = *severity;
            assert_eq!(
                notification_type(&req, &settings),
                *expected,
                "{event:?} with severity {severity:?}"
            );
        }
    }

    #[test]
    fn every_event_type_renders_without_failing() {
        let settings = Settings {
            notification_type: None,
            ..stateful_settings()
        };
        for event in general_notification_events() {
            let mut warnings = Vec::new();
            let payload = build_payload(&request(event), &settings, &mut warnings);
            assert!(
                payload["body"].as_str().is_some_and(|b| !b.is_empty()),
                "{event:?} rendered an empty body"
            );
            assert!(
                ["info", "success", "warning", "failure"]
                    .contains(&payload["type"].as_str().unwrap_or_default()),
                "{event:?} produced an Apprise type the API does not accept: {payload}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Settings validation (H1)
    // -----------------------------------------------------------------------

    #[test]
    fn the_two_routes_are_mutually_exclusive_and_one_is_required() {
        let mut warnings = Vec::new();
        let neither = resolve_route(None, None, true, &mut warnings).expect_err("neither");
        assert_eq!(neither.code, PluginErrorCode::InvalidConfig);
        assert!(neither.public_message.contains("configuration_key"));
        assert!(neither.public_message.contains("stateless_urls"));

        let both =
            resolve_route(Some("key"), Some("json://x"), true, &mut warnings).expect_err("both");
        assert_eq!(both.code, PluginErrorCode::InvalidConfig);
        assert!(both.public_message.contains("not both"));

        assert_eq!(
            resolve_route(Some("key"), None, true, &mut warnings).expect("stateful"),
            Route::Stateful {
                key: "key".to_string()
            }
        );
    }

    /// `api/urls.py` routes `[\w_-]{1,128}`; Sonarr's `^[a-z0-9-]*$` is
    /// narrower and rejects keys the server serves.
    #[test]
    fn the_configuration_key_charset_is_the_apprise_route_not_sonarrs() {
        for accepted in ["scryer", "MyKey", "home_lab", "a-b_C9", &"k".repeat(128)] {
            assert!(
                validated_configuration_key(accepted).is_ok(),
                "{accepted} must be accepted"
            );
        }
        for rejected in ["has space", "dots.are.out", "slash/es", "col:on", "über"] {
            let error = validated_configuration_key(rejected).expect_err(rejected);
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(error.public_message.contains("configuration_key"));
        }
        let long = validated_configuration_key(&"k".repeat(129)).expect_err("too long");
        assert!(long.public_message.contains("128"));
    }

    #[test]
    fn stateless_urls_are_normalised_and_checked() {
        let mut warnings = Vec::new();
        assert_eq!(
            validated_stateless_urls(
                "  discord://a/b \n mailto://c@d.test\n\n , json://e ",
                true,
                &mut warnings
            )
            .expect("normalised"),
            "discord://a/b,mailto://c@d.test,json://e"
        );
        assert!(warnings.is_empty(), "{warnings:?}");

        let empty = validated_stateless_urls("   \n  ", true, &mut warnings).expect_err("empty");
        assert_eq!(empty.code, PluginErrorCode::InvalidConfig);

        // Strict (test time) refuses; a live send warns and keeps delivering to
        // the entries that are usable.
        let mut warnings = Vec::new();
        let strict = validated_stateless_urls("discord://a/b, notaurl", true, &mut warnings)
            .expect_err("strict");
        assert_eq!(strict.code, PluginErrorCode::InvalidConfig);
        assert!(strict.public_message.contains("notaurl"));

        let mut warnings = Vec::new();
        let lenient = validated_stateless_urls("discord://a/b, notaurl", false, &mut warnings)
            .expect("lenient");
        assert_eq!(lenient, "discord://a/b,notaurl");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("notaurl"));
    }

    #[test]
    fn the_server_url_must_be_absolute() {
        assert_eq!(
            normalized_server("https://apprise.test:8000/ ".to_string()).expect("normalised"),
            "https://apprise.test:8000"
        );
        for rejected in ["apprise.test:8000", "ftp://apprise.test", "https://"] {
            let error = normalized_server(rejected.to_string()).expect_err(rejected);
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(error.public_message.contains("server_url"));
        }
    }

    #[test]
    fn tags_are_validated_against_apprises_expression_charset() {
        assert_eq!(
            validated_tags(&[
                "family".to_string(),
                "phone tablet".to_string(),
                "3:high".to_string(),
                "family".to_string(),
            ])
            .expect("valid"),
            vec![
                "family".to_string(),
                "phone tablet".to_string(),
                "3:high".to_string()
            ]
        );
        let error = validated_tags(&["bad!tag".to_string()]).expect_err("invalid");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("tags"));
    }

    #[test]
    fn an_unknown_notification_type_is_a_configuration_error() {
        assert_eq!(
            validated_notification_type("Failure").expect("known"),
            Some("failure".to_string())
        );
        assert_eq!(validated_notification_type("auto").expect("auto"), None);
        let error = validated_notification_type("urgent").expect_err("unknown");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("notification_type"));
    }

    // -----------------------------------------------------------------------
    // Delivery classification (H1)
    // -----------------------------------------------------------------------

    fn classify(
        status: u16,
        body: &str,
        route: &Route,
    ) -> PluginResult<PluginNotificationResponse> {
        classify_response(status, &BTreeMap::new(), body.as_bytes(), route, Vec::new())
    }

    fn expect_error(result: PluginResult<PluginNotificationResponse>) -> PluginError {
        match result {
            PluginResult::Err(error) => error,
            PluginResult::Ok(response) => {
                panic!("expected a typed error, got {response:?}")
            }
        }
    }

    fn expect_ok(result: PluginResult<PluginNotificationResponse>) -> PluginNotificationResponse {
        match result {
            PluginResult::Ok(response) => response,
            PluginResult::Err(error) => panic!("expected a delivery result, got {error:?}"),
        }
    }

    #[test]
    fn a_200_is_a_delivery_with_the_route_recorded() {
        let response = expect_ok(classify(
            200,
            r#"{"error": null, "details": []}"#,
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert!(response.success);
        assert!(response.warnings.is_empty(), "{:?}", response.warnings);
        assert_eq!(response.target_results.len(), 1);
        assert_eq!(response.target_results[0].target, "notify/scryer");
        assert!(response.target_results[0].success);
        assert_eq!(
            response.target_results[0].status.as_deref(),
            Some("http_200")
        );
    }

    /// The bug Sonarr cannot see: `204` is a 2xx, so its `HttpClient` does not
    /// throw and both the send and the connection test report success while
    /// nothing was delivered.
    #[test]
    fn a_204_is_a_configuration_error_naming_the_route() {
        let error = expect_error(classify(
            204,
            r#"{"error": "There was no configuration found", "details": []}"#,
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("configuration_key"));
        assert!(error.public_message.contains("scryer"));

        let error = expect_error(classify(
            204,
            r#"{"error": "There was no valid URLs provided to notify"}"#,
            &Route::Stateless {
                urls: "json://x".to_string(),
            },
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("stateless_urls"));
    }

    /// `424` is the API saying at least one destination refused. It is a
    /// delivery outcome, not a settings problem.
    #[test]
    fn a_424_is_a_reported_delivery_failure_carrying_the_servers_own_words() {
        let response = expect_ok(classify(
            424,
            r#"{"error": "One or more notification could not be sent", "details": [["INFO","2026-09-02","Sending"],["ERROR","2026-09-02","Failed to send Discord notification."]]}"#,
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_424"));
        let error = response.error.expect("an error message");
        assert!(
            error.contains("Failed to send Discord notification."),
            "{error}"
        );
        assert!(
            !error.contains("Sending"),
            "info lines must not be quoted: {error}"
        );
        assert_eq!(response.target_results.len(), 1);
        assert!(!response.target_results[0].success);
    }

    #[test]
    fn credentials_are_the_only_thing_a_401_can_be_about() {
        for status in [401, 403] {
            let error = expect_error(classify(
                status,
                r#"{"error": "denied"}"#,
                &Route::Stateful {
                    key: "scryer".to_string(),
                },
            ));
            assert_eq!(error.code, PluginErrorCode::AuthFailed);
            assert!(error.public_message.contains("auth_username"));
        }
        // A proxy's HTML 401 is still an auth failure, not "this is not Apprise".
        let error = expect_error(classify(
            401,
            "<html>Unauthorized</html>",
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
    }

    #[test]
    fn a_404_names_the_server_url_and_the_endpoint() {
        let error = expect_error(classify(
            404,
            r#"{"error": "not found"}"#,
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
        assert!(error.public_message.contains("/notify/scryer"));
    }

    #[test]
    fn a_rejected_request_is_this_plugins_fault() {
        for status in [400, 405, 406, 431] {
            let error = expect_error(classify(
                status,
                r#"{"error": "Invalid payload"}"#,
                &Route::Stateful {
                    key: "scryer".to_string(),
                },
            ));
            assert_eq!(error.code, PluginErrorCode::Permanent, "HTTP {status}");
            assert!(error.public_message.contains("Invalid payload"));
        }
    }

    #[test]
    fn a_5xx_is_the_delivery_lane_and_honours_retry_after() {
        let mut headers = BTreeMap::new();
        headers.insert("Retry-After".to_string(), "30".to_string());
        let response = expect_ok(classify_response(
            503,
            &headers,
            br#"{"error": "unavailable"}"#,
            &Route::Stateful {
                key: "scryer".to_string(),
            },
            Vec::new(),
        ));
        assert!(!response.success);
        assert_eq!(response.retry_after_seconds, Some(30));
        assert_eq!(response.provider_status.as_deref(), Some("http_503"));
    }

    #[test]
    fn a_429_from_a_proxy_is_a_delivery_failure_with_a_retry_window() {
        let mut headers = BTreeMap::new();
        headers.insert("retry-after".to_string(), "0".to_string());
        let response = expect_ok(classify_response(
            429,
            &headers,
            b"<html>slow down</html>",
            &Route::Stateful {
                key: "scryer".to_string(),
            },
            Vec::new(),
        ));
        assert!(!response.success);
        // A `Retry-After: 0` still means "wait", so it is floored at one second.
        assert_eq!(response.retry_after_seconds, Some(1));
    }

    /// A gateway's HTML `502` stays in the delivery lane: it is a transient
    /// upstream, not evidence that `server_url` is wrong.
    #[test]
    fn a_non_json_5xx_stays_in_the_delivery_lane() {
        let response = expect_ok(classify(
            502,
            "<html>Bad Gateway</html>",
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_502"));
    }

    #[test]
    fn a_non_json_4xx_is_a_wrong_server_url() {
        let error = expect_error(classify(
            418,
            "<html>I am a teapot</html>",
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
    }

    #[test]
    fn a_non_json_2xx_delivers_with_a_warning() {
        let response = expect_ok(classify(
            200,
            "OK",
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert!(response.success);
        assert_eq!(response.warnings.len(), 1, "{:?}", response.warnings);
        assert!(response.warnings[0].contains("apprise-api"));
    }

    #[test]
    fn warnings_logged_alongside_a_success_reach_the_operator() {
        let response = expect_ok(classify(
            200,
            r#"{"error": null, "details": [["WARNING","2026-09-02","Skipped mailto:// (no recipients)"]]}"#,
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert!(response.success);
        assert_eq!(response.warnings, vec!["Skipped mailto:// (no recipients)"]);
    }

    #[test]
    fn upstream_text_is_bounded() {
        let long = "x".repeat(1000);
        let body = format!(r#"{{"error": "{long}"}}"#);
        let error = expect_error(classify(
            400,
            &body,
            &Route::Stateful {
                key: "scryer".to_string(),
            },
        ));
        assert!(error.public_message.chars().count() < 400);
        assert!(error.public_message.ends_with('…'));
    }

    #[test]
    fn a_stateless_target_reports_schemes_and_never_credentials() {
        let target = Route::Stateless {
            urls: "mailto://user:hunter2@example.test,discord://id/token".to_string(),
        }
        .target();
        assert!(target.contains("mailto://"));
        assert!(target.contains("discord://"));
        assert!(!target.contains("hunter2"));
        assert!(!target.contains("token"));
    }

    // -----------------------------------------------------------------------
    // Test-time probes
    // -----------------------------------------------------------------------

    #[test]
    fn a_healthy_status_probe_says_nothing() {
        let body = br#"{"config_lock": false, "attach_lock": false, "stateful_enabled": true, "max_attachments": 4, "attach_size": 200, "status": "OK"}"#;
        assert!(status_warnings(200, body, &stateful_settings()).is_empty());
    }

    #[test]
    fn a_status_probe_reports_disabled_stateful_storage_and_attachments() {
        let body = br#"{"config_lock": true, "attach_lock": true, "stateful_enabled": false, "max_attachments": 0, "attach_size": 0, "status": "OK"}"#;
        let settings = Settings {
            include_poster: true,
            ..stateful_settings()
        };
        let warnings = status_warnings(200, body, &settings);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("APPRISE_STATEFUL_MODE")));
        assert!(warnings.iter().any(|w| w.contains("APPRISE_ATTACH_SIZE")));

        // Stateless routes do not care about stateful storage.
        let stateless = Settings {
            include_poster: false,
            ..stateless_settings()
        };
        assert!(status_warnings(200, body, &stateless).is_empty());
    }

    #[test]
    fn an_unhealthy_or_unrecognised_status_probe_warns_but_never_fails() {
        let unhealthy = status_warnings(
            417,
            br#"{"stateful_enabled": true, "status": "Could not write to storage"}"#,
            &stateful_settings(),
        );
        assert_eq!(unhealthy.len(), 1, "{unhealthy:?}");
        assert!(unhealthy[0].contains("Could not write to storage"));

        let foreign = status_warnings(200, b"<html>hello</html>", &stateful_settings());
        assert_eq!(foreign.len(), 1, "{foreign:?}");
        assert!(foreign[0].contains("apprise-api"));
    }

    #[test]
    fn a_tag_probe_reports_tags_the_configuration_does_not_carry() {
        let body = br#"{"tags": ["family", "phone"], "urls": []}"#;
        let warnings = tag_warnings(
            200,
            body,
            &[
                "family".to_string(),
                "phone tablet".to_string(),
                "3:desktop".to_string(),
            ],
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("'tablet'")));
        assert!(warnings.iter().any(|w| w.contains("'desktop'")));

        assert!(tag_warnings(200, body, &["family".to_string()]).is_empty());
        assert!(tag_warnings(200, body, &["all".to_string()]).is_empty());
        // An empty stored configuration is worth saying out loud.
        let empty = tag_warnings(204, b"", &["family".to_string()]);
        assert_eq!(empty.len(), 1, "{empty:?}");
        // Anything unrecognised stays silent.
        assert!(tag_warnings(500, b"boom", &["family".to_string()]).is_empty());
    }

    // -----------------------------------------------------------------------
    // Rendering details
    // -----------------------------------------------------------------------

    #[test]
    fn a_failed_download_renders_the_client_and_status_and_never_a_destination() {
        let mut req = request(NotificationEventType::Download);
        req.severity = Some(NotificationSeverity::Error);
        req.summary_message = "Download failed: Example Show".to_string();
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            status: Some("failed".to_string()),
            status_message: Some("all articles missing".to_string()),
            ..PluginNotificationDownload::default()
        });
        req.import = Some(PluginNotificationImport {
            dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
            ..PluginNotificationImport::default()
        });
        let rendered = body(&req);
        assert!(rendered.contains("Client: Weaver"));
        assert!(rendered.contains("Status: all articles missing"));
        assert!(!rendered.contains("Destination"), "{rendered}");
    }

    #[test]
    fn a_delete_renders_the_deleted_path() {
        let mut req = request(NotificationEventType::FileDeleted);
        req.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Example Show/old.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });
        assert!(body(&req).contains("File: /media/TV/Example Show/old.mkv"));
    }

    #[test]
    fn health_and_update_events_render_their_own_blocks() {
        let mut health = request(NotificationEventType::HealthIssue);
        health.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable".to_string()),
            ..PluginNotificationHealth::default()
        });
        let rendered = body(&health);
        assert!(rendered.contains("Check: IndexerStatusCheck"));
        assert!(rendered.contains("Detail: Indexers unavailable"));

        let mut update = request(NotificationEventType::ApplicationUpdate);
        update.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            ..PluginNotificationApplicationUpdate::default()
        });
        let rendered = body(&update);
        assert!(rendered.contains("Previous Version: 0.19.7"));
        assert!(rendered.contains("New Version: 0.19.8"));
    }

    #[test]
    fn a_manual_interaction_renders_its_link_only_when_it_is_absolute() {
        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            reason: Some("Sample file".to_string()),
            link: Some("/activity/queue".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        assert!(!body(&req).contains("Link:"));
        req.manual_interaction.as_mut().expect("interaction").link =
            Some("https://scryer.test/activity/queue".to_string());
        assert!(body(&req).contains("Link: https://scryer.test/activity/queue"));
    }

    #[test]
    fn subtitle_languages_come_from_the_media_files() {
        let mut req = request(NotificationEventType::SubtitleDownloaded);
        req.media_files = vec![PluginNotificationMediaFile {
            path: "/media/TV/Example Show/S01E01.mkv".to_string(),
            subtitle_languages: vec!["English".to_string(), "Dutch".to_string()],
            ..PluginNotificationMediaFile::default()
        }];
        assert!(body(&req).contains("Languages: English, Dutch"));
    }

    #[test]
    fn episode_display_is_composed_when_the_core_leaves_it_empty() {
        let mut req = request(NotificationEventType::Grab);
        req.episodes = vec![
            PluginNotificationEpisode {
                season_number: Some("1".to_string()),
                episode_number: Some("1".to_string()),
                title: Some("Pilot".to_string()),
                ..PluginNotificationEpisode::default()
            },
            PluginNotificationEpisode {
                season_number: Some("1".to_string()),
                episode_number: Some("2".to_string()),
                title: Some("Second".to_string()),
                ..PluginNotificationEpisode::default()
            },
        ];
        assert_eq!(
            episode_display(&req).as_deref(),
            Some("1x01x02 - Pilot + Second")
        );

        let mut daily = request(NotificationEventType::Grab);
        daily.episode = Some(PluginNotificationEpisode {
            air_date: Some("2026-09-02".to_string()),
            title: Some("Tonight".to_string()),
            ..PluginNotificationEpisode::default()
        });
        assert_eq!(
            episode_display(&daily).as_deref(),
            Some("2026-09-02 - Tonight")
        );
    }

    #[test]
    fn sizes_round_the_way_sonarr_rounds_them() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
        assert_eq!(format_bytes(1_610_612_736), "1.5 GB");
    }

    #[test]
    fn the_descriptor_is_a_notification_channel_with_the_documented_fields() {
        let descriptor = build_descriptor();
        assert_eq!(descriptor.id, PROVIDER_TYPE);
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("not a notification descriptor");
        };
        assert_eq!(notification.provider_type, PROVIDER_TYPE);
        assert!(!notification.capabilities.requires_host_filesystem);
        assert!(!notification.capabilities.requires_host_process);
        assert!(notification.capabilities.supports_images);

        // Config keys are a public contract.
        let keys: Vec<&str> = notification
            .config_fields
            .iter()
            .map(|field| field.key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![
                "server_url",
                "configuration_key",
                "stateless_urls",
                "notification_type",
                "tags",
                "include_poster",
                "auth_username",
                "auth_password",
            ]
        );

        let tags = notification
            .config_fields
            .iter()
            .find(|field| field.key == "tags")
            .expect("a tags field");
        assert_eq!(tags.field_type, ConfigFieldType::Tag);

        let notification_type = notification
            .config_fields
            .iter()
            .find(|field| field.key == "notification_type")
            .expect("a notification_type field");
        assert_eq!(notification_type.default_value.as_deref(), Some("info"));
        assert!(
            notification_type
                .options
                .iter()
                .any(|option| option.value == NOTIFICATION_TYPE_AUTO)
        );
    }
}
