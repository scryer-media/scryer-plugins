//! Mailgun email notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Mailgun notification (`src/NzbDrone.Core/Notifications/Mailgun/`) is
//! a four-parameter form POST: `from`, one `to` per recipient, `subject` and
//! `text` (`MailgunProxy.cs:44-66`). Its error handling is one branch — a 401
//! becomes "Unauthorised - ApiKey is invalid" and *every other* status becomes
//! "Unable to connect to Mailgun. Status code: {0}" (`MailgunProxy.cs:33-42`),
//! which is thrown into the log on a live send and only surfaced to the operator
//! from `Test` (`Mailgun.cs:81-100`). Its settings validator requires `ApiKey`,
//! `From` and `Recipients` to be non-empty and checks nothing else
//! (`MailgunSettings.cs:11-16`) — notably not `SenderDomain`, which is
//! interpolated straight into the request path.
//!
//! The June port copied that shape, kept the four parameters, made `recipients`
//! a free-text `String` instead of Sonarr's tag list, and reported an
//! unconfigured recipient list as a *delivery failure* (the old `lib.rs:95-97`).
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * every configuration problem is a typed `PluginError` naming the field —
//!   `api_key`, `sender_domain`, `from`, `recipients`, `cc`, `bcc`, `tags` —
//!   instead of a fake delivery failure or a generic connection error;
//! * Mailgun's own error body (`{"message": "…"}`) is parsed and attributed to
//!   the offending setting, so a rejected key is `AuthFailed` on `api_key`, an
//!   unknown domain is `InvalidConfig` on `sender_domain` (naming
//!   `use_eu_endpoint`, because a US domain asked for on the EU host answers
//!   404), and a sandbox domain's authorized-recipient rule is `InvalidConfig`
//!   on `recipients`;
//! * a 429 is a delivery failure carrying `retry_after_seconds`, and a 5xx is a
//!   delivery failure rather than a configuration error — Sonarr cannot tell any
//!   of these apart;
//! * the message body is enriched per event from the structured blocks the
//!   contract carries (episode, quality, release, indexer, client, size, paths,
//!   health, versions), and an **HTML alternative** is sent alongside the plain
//!   text in the same message, which Mailgun supports natively and Sonarr never
//!   uses;
//! * `cc`, `bcc` and Mailgun's `o:tag` analytics tags are configurable, and the
//!   documented limits are enforced here with a warning (1,000 recipients per
//!   message; three tags of at most 128 ASCII characters) rather than being left
//!   for Mailgun to reject.
//!
//! # Why the delivery path is local rather than `notify_common::send_form`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")`. Mailgun's failures are three different lanes in Scryer's
//! contract: a 401/403/404 names a setting the operator has to fix, a 429 or a
//! 402 is the account saying "not now", and a 5xx is Mailgun being briefly
//! unavailable. The helper also cannot read `Retry-After`.
//!
//! # Upstream reference
//!
//! Read 2026-09-02:
//! * <https://documentation.mailgun.com/docs/mailgun/api-reference/send/mailgun/messages>
//!   — `POST /v3/{domain_name}/messages`, the form fields (`from`, `to`, `cc`,
//!   `bcc`, `subject`, `text`, `html`, `o:*`, `h:X-*`, `v:*`), the
//!   `{"id","message"}` success body, and the 16 KB ceiling on all `o:`/`h:`/
//!   `v:`/`t:` options combined.
//! * <https://documentation.mailgun.com/docs/mailgun/api-reference/api-overview>
//!   — the documented status codes (400/401/403/404/429/500), the `{"message"}`
//!   error body, the US and EU base URLs, and the
//!   `X-RateLimit-Limit`/`-Remaining`/`-Reset` headers (`Reset` is Unix
//!   **milliseconds**).
//! * <https://documentation.mailgun.com/docs/mailgun/api-reference/mg-auth> —
//!   HTTP Basic with the fixed user `api`; a *domain sending key* may call only
//!   `/messages` and `/messages.mime` for its own domain, which is why the
//!   Test-time domain probe below can only ever produce warnings.
//! * <https://documentation.mailgun.com/docs/mailgun/user-manual/domains/domains-sandbox>
//!   and <https://help.mailgun.com/hc/en-us/articles/217531258-Authorized-Recipients>
//!   — a `sandbox….mailgun.org` domain delivers only to verified authorized
//!   recipients, which is the single most common failure on a new account.
//! * <https://documentation.mailgun.com/docs/mailgun/user-manual/tracking-messages/track-tagging>
//!   — at most three tags per message, 128 ASCII characters each.
//! * <https://documentation.mailgun.com/docs/mailgun/user-manual/sending-messages/send-http>
//!   — 25 MB maximum message size, 1,000 recipients per request.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, PluginNotificationEpisode,
    PluginNotificationTargetResult, current_sdk_constraint,
};
use serde_json::Value;

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

const PROVIDER_TYPE: &str = "mailgun";
const USER_AGENT: &str = concat!("scryer-mailgun-plugin/", env!("CARGO_PKG_VERSION"));

/// `MailgunProxy.cs:15-16`, still current: the US and EU regions are separate
/// deployments with separate domain namespaces, so a domain registered in one
/// answers 404 in the other.
const US_BASE_URL: &str = "https://api.mailgun.net";
const EU_BASE_URL: &str = "https://api.eu.mailgun.net";
const US_HOST: &str = "api.mailgun.net";
const EU_HOST: &str = "api.eu.mailgun.net";

/// Sending is `v3`; the Domains API this channel probes at Test time is `v4`.
/// Sonarr's proxy pins `v3` into its base URL for everything.
const MESSAGES_API_VERSION: &str = "v3";
const DOMAINS_API_VERSION: &str = "v4";

// ---------------------------------------------------------------------------
// Mailgun's documented limits
// ---------------------------------------------------------------------------

/// "Maximum 1,000 recipients per batch" — counted across `to`, `cc` and `bcc`.
const RECIPIENT_LIMIT: usize = 1_000;
/// "A single message may be marked with up to 3 tags."
const TAG_LIMIT: usize = 3;
/// "the maximum length of characters is 128 […] tags […] should be ascii only".
const TAG_CHARACTER_LIMIT: usize = 128;

/// A plugin-side ceiling on any single rendered value, far below Mailgun's 25 MB
/// message limit. Its job is to stop one pathological path or status string from
/// turning a notification into a megabyte of email, not to approximate the API's
/// own limit.
const VALUE_CHARACTER_LIMIT: usize = 2_000;
/// The same idea for the event summary, which is prose and legitimately longer.
const SUMMARY_CHARACTER_LIMIT: usize = 8_000;
/// `subject` is a mail header; anything past this is unreadable in every client.
const SUBJECT_CHARACTER_LIMIT: usize = 255;

/// The subject a request with no summary title still has to carry: Mailgun
/// documents `subject` as required.
const FALLBACK_SUBJECT: &str = "Scryer Notification";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Mailgun".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // Deliberately unset: the base URL is not an operator setting here,
            // it is derived from `use_eu_endpoint`, and prefilling one of the
            // two regions would be documentation that is wrong half the time.
            default_base_url: None,
            allowed_hosts: vec![US_HOST.to_string(), EU_HOST.to_string()],
            capabilities: NotificationCapabilities {
                // The message carries an HTML alternative alongside the plain
                // text (`html` and `text` in one Mailgun message).
                supports_rich_text: true,
                // The HTML part embeds the title's poster when the contract
                // carries one; a mail client fetches it like any remote image.
                supports_images: true,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![NotificationDeliveryMode::Email],
                payload_formats: vec![
                    NotificationPayloadFormat::PlainText,
                    NotificationPayloadFormat::Html,
                ],
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
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some(
                "A Mailgun private API key, or a domain sending key for the sending domain below.",
            ),
        ),
        field(
            "use_eu_endpoint",
            "Use EU Endpoint",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Send through api.eu.mailgun.net instead of api.mailgun.net. Must match the region the sending domain was created in.",
            ),
        ),
        field(
            "sender_domain",
            "Sender Domain",
            ConfigFieldType::String,
            true,
            None,
            Some("The Mailgun sending domain, for example mg.example.com."),
        ),
        field(
            "from",
            "From Address",
            ConfigFieldType::String,
            true,
            None,
            Some(
                "The sender address, optionally as a display name: Scryer <scryer@mg.example.com>.",
            ),
        ),
        // Sonarr models this as a `Tag` (`MailgunSettings.cs:40`); the June port
        // made it a free `String`. Scryer's notification settings UI renders a
        // `Tag` field as comma-separated text, so the stored value is unchanged
        // and every existing configuration keeps parsing.
        field(
            "recipients",
            "Recipients",
            ConfigFieldType::Tag,
            true,
            None,
            Some("Recipient email addresses."),
        ),
        field(
            "cc",
            "CC",
            ConfigFieldType::Tag,
            false,
            None,
            Some("Additional visible recipients."),
        ),
        field(
            "bcc",
            "BCC",
            ConfigFieldType::Tag,
            false,
            None,
            Some("Additional hidden recipients."),
        ),
        field(
            "tags",
            "Mailgun Tags",
            ConfigFieldType::Tag,
            false,
            None,
            Some(
                "Mailgun analytics tags applied to every message. At most three, 128 ASCII characters each.",
            ),
        ),
        field(
            "send_html",
            "Send HTML Alternative",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            Some(
                "Send an HTML part alongside the plain-text body. Turn this off for plain-text-only mailboxes.",
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Everything the renderer and the sender need from configuration, resolved and
/// validated once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    api_key: String,
    base_url: &'static str,
    domain: String,
    from: String,
    recipients: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    tags: Vec<String>,
    send_html: bool,
}

impl Settings {
    fn messages_url(&self) -> String {
        format!(
            "{}/{}/{}/messages",
            self.base_url,
            MESSAGES_API_VERSION,
            path_segment(&self.domain)
        )
    }

    fn domain_url(&self) -> String {
        format!(
            "{}/{}/domains/{}",
            self.base_url,
            DOMAINS_API_VERSION,
            path_segment(&self.domain)
        )
    }

    /// `strict` is the Test-time posture. Anything Mailgun itself will refuse
    /// outright — no API key, no sending domain, no usable sender, no usable
    /// recipient — is an error on every send. Anything that is only *probably*
    /// wrong — one malformed address among several, a tag outside Mailgun's
    /// documented character set, a sending domain that does not look like a
    /// hostname — fails the connection test and degrades to a warning on a live
    /// send, so a channel that works today keeps working if Mailgun ever widens
    /// a rule.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let mut warnings = Vec::new();

        let api_key = required_setting("api_key")?;
        let domain = validated_domain(&required_setting("sender_domain")?, strict, &mut warnings)?;
        let from = validated_from(&required_setting("from")?)?;

        let recipients = validated_addresses(
            "recipients",
            &config_csv("recipients"),
            true,
            strict,
            &mut warnings,
        )?;
        let cc = validated_addresses("cc", &config_csv("cc"), false, strict, &mut warnings)?;
        let bcc = validated_addresses("bcc", &config_csv("bcc"), false, strict, &mut warnings)?;

        let (recipients, cc, bcc) = enforce_recipient_limit(recipients, cc, bcc, &mut warnings);
        let tags = validated_tags(&config_csv("tags"), strict, &mut warnings)?;

        Ok((
            Self {
                api_key,
                base_url: if config_bool("use_eu_endpoint") {
                    EU_BASE_URL
                } else {
                    US_BASE_URL
                },
                domain,
                from,
                recipients,
                cc,
                bcc,
                tags,
                // Sonarr sends `text` only. The HTML alternative is opt-out
                // rather than opt-in because a `multipart/alternative` message
                // is what every mail client expects.
                send_html: config_value("send_html")
                    .map(|_| config_bool("send_html"))
                    .unwrap_or(true),
            },
            warnings,
        ))
    }
}

/// A required setting that is missing names itself, on the typed lane.
///
/// `MailgunSettings.cs:11-16` validates `ApiKey`, `From` and `Recipients` in the
/// settings form and leaves `SenderDomain` unchecked even though it is
/// interpolated into the request path.
fn required_setting(key: &'static str) -> Result<String, PluginError> {
    config_value(key).ok_or_else(|| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("{key} is not configured"),
            None,
        )
    })
}

/// The sending domain is a path segment, so a value carrying a scheme or a slash
/// would silently rewrite the endpoint. That is refused on every send; a value
/// that is merely implausible as a hostname is refused only at Test time.
fn validated_domain(
    raw: &str,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<String, PluginError> {
    let lowered = raw.trim().to_ascii_lowercase();
    let trimmed = lowered
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_matches('/')
        .trim()
        .to_string();

    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('@') {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "sender_domain must be a Mailgun sending domain such as mg.example.com; got {raw:?}"
            ),
            None,
        ));
    }

    let plausible = trimmed.contains('.')
        && trimmed.is_ascii()
        && trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'));
    if !plausible {
        if strict {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "sender_domain does not look like a hostname: {raw:?}. Mailgun answers 404 for a domain it does not have."
                ),
                None,
            ));
        }
        warnings.push(format!(
            "sender_domain {raw:?} does not look like a hostname; Mailgun may answer 404"
        ));
    }

    Ok(trimmed)
}

/// The `from` value Mailgun receives, either `local@domain` or
/// `Display Name <local@domain>`.
///
/// Sonarr requires it to be non-empty and nothing more (`MailgunSettings.cs:14`),
/// so a typo is a guaranteed 400 the operator only sees in the log.
fn validated_from(raw: &str) -> Result<String, PluginError> {
    parse_address(raw).ok_or_else(|| {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "from must be an email address, optionally with a display name (Scryer <scryer@mg.example.com>); got {raw:?}"
            ),
            None,
        )
    })
}

/// A recipient list, deduplicated case-insensitively on the address.
///
/// Mailgun ignores duplicate recipients itself; deduplicating here keeps the
/// per-target results honest and keeps the 1,000-recipient budget accurate.
fn validated_addresses(
    key: &'static str,
    raw: &[String],
    required: bool,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, PluginError> {
    let mut valid: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();

    for entry in raw {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some(address) = parse_address(entry) else {
            rejected.push(entry.to_string());
            continue;
        };
        let mailbox = mailbox_of(&address);
        if !valid
            .iter()
            .any(|existing| mailbox_of(existing).eq_ignore_ascii_case(&mailbox))
        {
            valid.push(address);
        }
    }

    if !rejected.is_empty() {
        // Every entry is unusable: there is nothing to degrade to, so this is a
        // configuration error whether or not it is a test.
        if strict || valid.is_empty() {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "{key} contains {} that {} not an email address: {}",
                    if rejected.len() == 1 {
                        "an entry".to_string()
                    } else {
                        format!("{} entries", rejected.len())
                    },
                    if rejected.len() == 1 { "is" } else { "are" },
                    rejected.join(", ")
                ),
                None,
            ));
        }
        warnings.push(format!(
            "{key} entries dropped, they are not email addresses: {}",
            rejected.join(", ")
        ));
    }

    if required && valid.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("{key} must list at least one email address"),
            None,
        ));
    }

    Ok(valid)
}

/// "Maximum 1,000 recipients per batch", counted across `to`, `cc` and `bcc`.
/// The visible recipients are kept first: dropping a `bcc` is less surprising
/// than dropping the addressee.
fn enforce_recipient_limit(
    mut recipients: Vec<String>,
    mut cc: Vec<String>,
    mut bcc: Vec<String>,
    warnings: &mut Vec<String>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let total = recipients.len() + cc.len() + bcc.len();
    if total <= RECIPIENT_LIMIT {
        return (recipients, cc, bcc);
    }

    warnings.push(format!(
        "{total} recipients configured but Mailgun accepts at most {RECIPIENT_LIMIT} per message; the last {} were dropped",
        total - RECIPIENT_LIMIT
    ));

    recipients.truncate(RECIPIENT_LIMIT);
    let mut budget = RECIPIENT_LIMIT - recipients.len();
    cc.truncate(budget);
    budget -= cc.len();
    bcc.truncate(budget);
    (recipients, cc, bcc)
}

/// `o:tag`: at most three per message, 128 ASCII characters each.
fn validated_tags(
    raw: &[String],
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, PluginError> {
    let mut valid: Vec<String> = Vec::new();
    for tag in raw {
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }
        let well_formed = tag.is_ascii()
            && tag.chars().count() <= TAG_CHARACTER_LIMIT
            && !tag.chars().any(|character| character.is_control());
        if !well_formed {
            if strict {
                return Err(plugin_error(
                    PluginErrorCode::InvalidConfig,
                    format!(
                        "tags contains {tag:?}, which Mailgun cannot accept: a tag is at most {TAG_CHARACTER_LIMIT} ASCII characters"
                    ),
                    None,
                ));
            }
            warnings.push(format!(
                "tag {tag:?} dropped: Mailgun accepts at most {TAG_CHARACTER_LIMIT} ASCII characters per tag"
            ));
            continue;
        }
        if !valid
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(tag))
        {
            valid.push(tag.to_string());
        }
    }

    if valid.len() > TAG_LIMIT {
        if strict {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "tags lists {} entries but Mailgun accepts at most {TAG_LIMIT} per message",
                    valid.len()
                ),
                None,
            ));
        }
        warnings.push(format!(
            "only the first {TAG_LIMIT} of {} tags were sent; Mailgun accepts no more",
            valid.len()
        ));
        valid.truncate(TAG_LIMIT);
    }

    Ok(valid)
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Parse and re-render one address, keeping a display name when there is one.
///
/// Deliberately not an RFC 5322 parser: the job is to reject what Mailgun will
/// certainly refuse — no `@`, an empty local part or domain, a domain with no
/// dot, embedded whitespace, a separator that means the operator wrote two
/// addresses in one field, or a header-injecting newline — while letting
/// everything plausible through. `notify_common::config_csv` has already split
/// on `,`, `;` and newlines, so a surviving separator is a real mistake.
fn parse_address(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(['\r', '\n', '\t']) {
        return None;
    }

    let (display, mailbox) = match (raw.rfind('<'), raw.rfind('>')) {
        (Some(open), Some(close)) if close > open => {
            (raw[..open].trim().to_string(), raw[open + 1..close].trim())
        }
        (None, None) => (String::new(), raw),
        // A lone angle bracket is a typo, not an address.
        _ => return None,
    };

    if !is_mailbox(mailbox) {
        return None;
    }

    let display = display.trim_matches('"').trim();
    if display.is_empty() {
        Some(mailbox.to_string())
    } else if display.contains(['<', '>', '"', ',', ';']) {
        // An unquotable display name is dropped rather than sent as a malformed
        // header; the address itself is what matters.
        Some(mailbox.to_string())
    } else {
        Some(format!("{display} <{mailbox}>"))
    }
}

fn is_mailbox(value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return false;
    }
    if value.contains([',', ';', '<', '>', '"']) {
        return false;
    }
    let mut parts = value.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// The bare address inside a rendered `Display <addr>` value.
fn mailbox_of(address: &str) -> String {
    match (address.rfind('<'), address.rfind('>')) {
        (Some(open), Some(close)) if close > open => address[open + 1..close].trim().to_string(),
        _ => address.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Sonarr sends a fixed constant per event ("Episode Grabbed", "Import
/// Complete", …) as the subject and puts everything else in the body
/// (`Mailgun.cs:25-79`). Scryer's dispatcher already composes an event-specific,
/// title-bearing heading in `summary_title` ("Grabbed: Example Show"), which is
/// strictly more useful as a subject line.
fn subject(req: &PluginNotificationRequest, warnings: &mut Vec<String>) -> String {
    let title = req.summary_title.trim();
    let subject = if !title.is_empty() {
        title
    } else if !req.app.name.trim().is_empty() {
        req.app.name.trim()
    } else {
        FALLBACK_SUBJECT
    };
    // A subject is a header: a newline in it would be header injection.
    let subject: String = subject
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let subject = subject.trim();
    if subject.chars().count() > SUBJECT_CHARACTER_LIMIT {
        warnings.push(format!(
            "the subject was trimmed to {SUBJECT_CHARACTER_LIMIT} characters"
        ));
    }
    ellipsize(subject, SUBJECT_CHARACTER_LIMIT)
}

/// The structured enrichment Sonarr's Mailgun channel has no room for: Sonarr
/// hands the proxy one prose sentence, while Scryer's contract carries the facts
/// separately. Every line is conditional on the block actually being present, so
/// the sparse shape the core sends today renders exactly the summary the June
/// port sent, and nothing more.
fn detail_lines(
    req: &PluginNotificationRequest,
    warnings: &mut Vec<String>,
) -> Vec<(&'static str, String)> {
    let mut lines: Vec<(&'static str, String)> = Vec::new();
    let mut push =
        |lines: &mut Vec<(&'static str, String)>, label: &'static str, value: Option<String>| {
            if let Some(value) = value.map(|value| value.trim().to_string())
                && !value.is_empty()
            {
                if value.chars().count() > VALUE_CHARACTER_LIMIT {
                    warnings.push(format!(
                        "{label} was trimmed to {VALUE_CHARACTER_LIMIT} characters"
                    ));
                }
                lines.push((label, ellipsize(&value, VALUE_CHARACTER_LIMIT)));
            }
        };

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
        // this arm renders a failure and never a destination path.
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

    // Every event carries these when the core filled them, and an email is the
    // one channel with room for them.
    push(&mut lines, "Title", title_name(req));
    push(
        &mut lines,
        "Event",
        Some(req.event_type.as_str().to_string()),
    );
    lines
}

/// The plain-text part.
///
/// The first paragraph is exactly what the June port sent — `summary_message`,
/// trimmed — so nothing an operator reads today disappears. The structured
/// detail is appended after a blank line, and the same facts go into the HTML
/// part, because a `multipart/alternative` message whose two parts say different
/// things is a bug in every mail client.
fn text_body(
    req: &PluginNotificationRequest,
    subject: &str,
    lines: &[(&'static str, String)],
    warnings: &mut Vec<String>,
) -> String {
    let mut out = String::new();
    let summary = req.summary_message.trim();
    if !summary.is_empty() {
        if summary.chars().count() > SUMMARY_CHARACTER_LIMIT {
            warnings.push(format!(
                "the message body was trimmed to {SUMMARY_CHARACTER_LIMIT} characters"
            ));
        }
        out.push_str(&ellipsize(summary, SUMMARY_CHARACTER_LIMIT));
    }

    if !lines.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        for (label, value) in lines {
            out.push_str(label);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        // Trailing newline from the loop.
        out.pop();
    }

    if out.is_empty() {
        // Mailgun needs at least one of `text`, `html` or `template`, and an
        // empty body is not one.
        out.push_str(subject);
    }
    out
}

/// The HTML alternative Sonarr never sends.
///
/// Deliberately one small inline-styled table and no external CSS: mail clients
/// strip `<style>` blocks, and a Scryer notification is a handful of labelled
/// facts, not a newsletter.
fn html_body(
    req: &PluginNotificationRequest,
    subject: &str,
    lines: &[(&'static str, String)],
) -> String {
    let mut out = String::from(
        "<html><body style=\"margin:0;padding:16px;font-family:-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;font-size:14px;color:#1b1b1f;background:#ffffff;\">",
    );

    out.push_str(&format!(
        "<h2 style=\"margin:0 0 12px;font-size:18px;font-weight:600;\">{}</h2>",
        html_escape(subject)
    ));

    if let Some(poster) = poster_url(req) {
        // A remote image the mail client fetches; `poster_url` is already the
        // contract's own URL, and an unreachable one degrades to the alt text.
        out.push_str(&format!(
            "<img src=\"{}\" alt=\"{}\" width=\"180\" style=\"max-width:180px;height:auto;border-radius:6px;margin:0 0 12px;\"/>",
            html_escape(&poster),
            html_escape(&title_name(req).unwrap_or_default())
        ));
    }

    let summary = req.summary_message.trim();
    if !summary.is_empty() {
        out.push_str(&format!(
            "<p style=\"margin:0 0 12px;\">{}</p>",
            html_escape(&ellipsize(summary, SUMMARY_CHARACTER_LIMIT)).replace('\n', "<br/>")
        ));
    }

    if !lines.is_empty() {
        out.push_str(
            "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"border-collapse:collapse;\">",
        );
        for (label, value) in lines {
            out.push_str(&format!(
                "<tr><td style=\"padding:2px 12px 2px 0;color:#5a5a66;white-space:nowrap;vertical-align:top;\">{}</td><td style=\"padding:2px 0;\">{}</td></tr>",
                html_escape(label),
                html_escape(value)
            ));
        }
        out.push_str("</table>");
    }

    let app = req.app.name.trim();
    out.push_str(&format!(
        "<p style=\"margin:16px 0 0;font-size:12px;color:#8a8a94;\">Sent by {}{}.</p>",
        html_escape(if app.is_empty() { "Scryer" } else { app }),
        if req.app.version.trim().is_empty() {
            String::new()
        } else {
            format!(" {}", html_escape(req.app.version.trim()))
        }
    ));

    out.push_str("</body></html>");
    out
}

fn ellipsize(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Field values
// ---------------------------------------------------------------------------

fn title_name(req: &PluginNotificationRequest) -> Option<String> {
    let title = req.title.as_ref()?;
    let name = title.name.trim();
    if name.is_empty() {
        return None;
    }
    Some(match title.year {
        Some(year) if year > 0 => format!("{name} ({year})"),
        _ => name.to_string(),
    })
}

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

/// The form parameters of one `POST /v3/{domain}/messages`, in the order Sonarr
/// writes them (`MailgunProxy.cs:54-62`) plus the ones it never sends.
///
/// Sonarr repeats `to` once per recipient; Mailgun accepts that and a
/// comma-separated list equally, and the repeated form is kept so the wire shape
/// stays the one Sonarr proved.
fn build_params(
    req: &PluginNotificationRequest,
    settings: &Settings,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut warnings = Vec::new();
    let subject = subject(req, &mut warnings);
    let lines = detail_lines(req, &mut warnings);
    let text = text_body(req, &subject, &lines, &mut warnings);

    let mut params = vec![("from".to_string(), settings.from.clone())];
    for recipient in &settings.recipients {
        params.push(("to".to_string(), recipient.clone()));
    }
    for recipient in &settings.cc {
        params.push(("cc".to_string(), recipient.clone()));
    }
    for recipient in &settings.bcc {
        params.push(("bcc".to_string(), recipient.clone()));
    }

    params.push(("subject".to_string(), subject.clone()));
    params.push(("text".to_string(), text));
    if settings.send_html {
        params.push(("html".to_string(), html_body(req, &subject, &lines)));
    }

    // `o:`/`h:`/`v:` options are capped at 16 KB in total; three 128-character
    // tags plus two short headers are nowhere near it.
    for tag in &settings.tags {
        params.push(("o:tag".to_string(), tag.clone()));
    }
    params.push((
        "h:X-Scryer-Event-Type".to_string(),
        req.event_type.as_str().to_string(),
    ));
    if let Some(event_id) = req
        .event_id
        .as_deref()
        .map(str::trim)
        .filter(|event_id| !event_id.is_empty())
    {
        params.push((
            "h:X-Scryer-Event-Id".to_string(),
            ellipsize(event_id, VALUE_CHARACTER_LIMIT),
        ));
    }

    (params, warnings)
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    // Test-time only, and everything it finds is a warning: the send that
    // follows produces the real error when the channel is genuinely wrong.
    if req.is_test {
        warnings.extend(probe_domain(&settings));
    }

    let request = HttpRequest::new(settings.messages_url())
        .with_method("POST")
        .with_header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT)
        .with_header("Authorization", basic_auth_header("api", &settings.api_key));

    let (params, payload_warnings) = build_params(req, &settings);
    warnings.extend(payload_warnings);

    match http::request::<Vec<u8>>(&request, Some(form_body(&params))) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            &settings,
            warnings,
        ),
        Err(error) => {
            // The host answers a refused or failed egress in-band; that is
            // Mailgun being unreachable, not a misconfigured channel.
            let detail = format!("request failed: {error}");
            let mut failure = error_response(detail.clone(), None);
            failure.target_results = target_results(&settings, false, None, Some(&detail));
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
    }
}

/// One entry per address the message was addressed to.
///
/// Mailgun accepts or refuses the whole message in one call and only reports
/// per-recipient outcomes later, through webhooks and the Events API, so every
/// entry necessarily shares the call's outcome. `status` says `queued` rather
/// than `delivered` for exactly that reason: a 200 from Mailgun is "Queued.
/// Thank you.", not a delivery.
fn target_results(
    settings: &Settings,
    success: bool,
    status: Option<String>,
    error: Option<&str>,
) -> Vec<PluginNotificationTargetResult> {
    settings
        .recipients
        .iter()
        .chain(settings.cc.iter())
        .chain(settings.bcc.iter())
        .map(|address| PluginNotificationTargetResult {
            target: mailbox_of(address),
            success,
            status: status.clone(),
            error: error.map(str::to_string),
        })
        .collect()
}

/// Mailgun's response body: `{"id": "<…>", "message": "Queued. Thank you."}` on
/// success, `{"message": "…"}` on an error.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MailgunBody {
    id: Option<String>,
    message: Option<String>,
    is_json: bool,
    raw: Option<String>,
}

impl MailgunBody {
    fn detail(&self, status: u16) -> String {
        if let Some(message) = self
            .message
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty())
        {
            return ellipsize(message, 300);
        }
        match self.raw.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => ellipsize(raw, 300),
            _ => format!("HTTP {status}"),
        }
    }

    fn mentions(&self, needles: &[&str]) -> bool {
        let Some(message) = self.message.as_deref() else {
            return false;
        };
        let message = message.to_ascii_lowercase();
        needles.iter().any(|needle| message.contains(needle))
    }
}

fn parse_mailgun_body(body: &[u8]) -> MailgunBody {
    let text = String::from_utf8_lossy(body).to_string();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return MailgunBody {
            raw: Some(text),
            ..MailgunBody::default()
        };
    };

    MailgunBody {
        id: map
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        message: map
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
        is_json: true,
        raw: Some(text),
    }
}

/// Sonarr turns a 401 into "Unauthorised - ApiKey is invalid" and *everything
/// else* into "Unable to connect to Mailgun. Status code: {0}"
/// (`MailgunProxy.cs:33-42`), thrown into the log on a live send. Scryer's typed
/// error lane exists on every send, so the operator is always told which setting
/// to fix — and the statuses that are not a configuration problem stay on the
/// delivery lane where they belong.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    settings: &Settings,
    mut warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let answer = parse_mailgun_body(body);
    let detail = answer.detail(status);
    let debug = format!("HTTP {status}: {detail}");

    if (200..300).contains(&status) {
        let mut response = ok_response();
        response.delivery_id = answer.id.clone();
        response.provider_status = Some("queued".to_string());
        if !answer.is_json {
            // Accepted, but not by something that answered like the Mailgun API:
            // an authenticating proxy or an unrelated service on that origin. A
            // warning rather than a failure — the message may well have been
            // accepted, and refusing a working channel over a response body
            // would be worse than saying so.
            warnings.push(format!(
                "the endpoint accepted the message with HTTP {status} but did not answer like the Mailgun API; check sender_domain and anything proxying api.mailgun.net"
            ));
        }
        response.target_results = target_results(settings, true, Some("queued".to_string()), None);
        response.warnings = warnings;
        return PluginResult::Ok(response);
    }

    match status {
        // "Unauthorised - ApiKey is invalid" (`MailgunProxy.cs:35-38`). Also the
        // answer when a *domain sending key* is used against a different domain,
        // or against the wrong region, so both are named.
        401 => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!(
                "api_key was rejected by Mailgun (HTTP 401): {detail}. Check the key, that a domain sending key belongs to sender_domain, and that use_eu_endpoint matches the region the key was created in."
            ),
            Some(debug),
        )),
        // A sandbox domain refuses anyone who is not a verified authorized
        // recipient — the single most common failure on a new Mailgun account.
        403 if answer.mentions(&["sandbox", "authorized recipient"])
            || settings.domain.starts_with("sandbox") =>
        {
            PluginResult::Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "Mailgun refused the message because {} is a sandbox domain (HTTP 403): {detail}. A sandbox domain delivers only to addresses added and verified as Authorized Recipients; add every entry in recipients, or use a verified sending domain.",
                    settings.domain
                ),
                Some(debug),
            ))
        }
        403 => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!(
                "Mailgun refused the request (HTTP 403): {detail}. The key in api_key is valid but not permitted to send for sender_domain."
            ),
            Some(debug),
        )),
        // The domain is not in this region's namespace.
        404 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "Mailgun has no domain named {} in this region (HTTP 404): {detail}. Check sender_domain, and use_eu_endpoint if the domain was created in the EU region.",
                settings.domain
            ),
            Some(debug),
        )),
        400 => PluginResult::Err(classify_bad_request(&answer, &detail, debug)),
        // The account cannot send: an unpaid invoice, a disabled account, or a
        // plan that does not allow this. Nothing in Scryer's configuration fixes
        // it, so it is a delivery outcome rather than a settings error.
        402 => {
            let mut failure = error_response(
                format!(
                    "Mailgun refused the message for an account or plan reason (HTTP 402): {detail}"
                ),
                Some("http_402".to_string()),
            );
            failure.target_results =
                target_results(settings, false, Some("http_402".to_string()), Some(&detail));
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        // The message this plugin built is too large for Mailgun's 25 MB limit.
        // The operator has nothing to fix; this is a plugin bug and is reported
        // as one.
        413 => PluginResult::Err(plugin_error(
            PluginErrorCode::Permanent,
            format!(
                "Mailgun rejected the message this plugin built as too large (HTTP 413): {detail}"
            ),
            Some(debug),
        )),
        429 => {
            let mut failure = error_response(
                format!("Mailgun rate-limited this channel (HTTP 429): {detail}"),
                Some("http_429".to_string()),
            );
            failure.retry_after_seconds = retry_after_seconds(headers);
            failure.target_results =
                target_results(settings, false, Some("http_429".to_string()), Some(&detail));
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        500..=599 => {
            let mut failure = error_response(
                format!("Mailgun is unavailable (HTTP {status}): {detail}"),
                Some(format!("http_{status}")),
            );
            failure.retry_after_seconds = retry_after_seconds(headers);
            failure.target_results = target_results(
                settings,
                false,
                Some(format!("http_{status}")),
                Some(&detail),
            );
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        _ => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("Mailgun rejected the request (HTTP {status}): {detail}"),
            Some(debug),
        )),
    }
}

/// Mailgun answers a 400 with a human-readable `{"message": …}`; the setting to
/// blame is in that sentence.
fn classify_bad_request(answer: &MailgunBody, detail: &str, debug: String) -> PluginError {
    if answer.mentions(&["'from'", "from parameter", "from address", "sender address"]) {
        return plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("from was rejected by Mailgun: {detail}"),
            Some(debug),
        );
    }
    if answer.mentions(&[
        "'to'",
        "to parameter",
        "recipient",
        "no valid address",
        "not a valid address",
    ]) {
        return plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("recipients was rejected by Mailgun: {detail}"),
            Some(debug),
        );
    }
    if answer.mentions(&["tag"]) {
        return plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("tags was rejected by Mailgun: {detail}"),
            Some(debug),
        );
    }
    if answer.mentions(&["domain"]) {
        return plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("sender_domain was rejected by Mailgun: {detail}"),
            Some(debug),
        );
    }
    // Nothing the operator configured is named, so the request this plugin built
    // is what Mailgun disliked.
    plugin_error(
        PluginErrorCode::Permanent,
        format!("Mailgun rejected the message this plugin built (HTTP 400): {detail}"),
        Some(debug),
    )
}

/// `Retry-After` in seconds when Mailgun (or a proxy) sends one; otherwise
/// `X-RateLimit-Reset`, which the API documents as Unix **milliseconds** UTC and
/// therefore has to be turned into a delay.
fn retry_after_seconds(headers: &BTreeMap<String, String>) -> Option<i64> {
    if let Some(seconds) =
        header(headers, "retry-after").and_then(|value| value.trim().parse::<i64>().ok())
    {
        return Some(seconds.max(1));
    }
    let reset_ms = header(headers, "x-ratelimit-reset")?
        .trim()
        .parse::<i64>()
        .ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((reset_ms / 1000 - now).max(1))
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

// ---------------------------------------------------------------------------
// Test-time probe
//
// Sonarr's `Test` is the send itself (`Mailgun.cs:81-100`), so it can only learn
// what a notification learns. One extra GET answers a question the send cannot:
// does this sending domain exist in this region, and is it verified? The Domains
// API moved to `v4`; `GET /v4/domains/{name}` reports `domain.state` as
// `active`, `unverified` or `disabled`.
//
// Everything it finds is a warning, and a probe that cannot decide says nothing.
// A *domain sending key* — the credential Mailgun recommends for exactly this
// use — may call only `/messages`, so a 401 or 403 here is expected and is not
// reported.
// ---------------------------------------------------------------------------

fn probe_domain(settings: &Settings) -> Vec<String> {
    let mut warnings = Vec::new();

    // A local check, no round trip: a sandbox domain delivers only to verified
    // authorized recipients, whatever the API says about it.
    if settings.domain.starts_with("sandbox") && settings.domain.ends_with(".mailgun.org") {
        warnings.push(format!(
            "{} is a Mailgun sandbox domain: it delivers only to addresses added and verified under Authorized Recipients, so notifications to anyone else will be refused",
            settings.domain
        ));
    }

    let request = HttpRequest::new(settings.domain_url())
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT)
        .with_header("Authorization", basic_auth_header("api", &settings.api_key));

    let Ok(response) = http::request::<Vec<u8>>(&request, None) else {
        return warnings;
    };

    let status = response.status_code();
    if status == 401 || status == 403 {
        // A domain sending key cannot read the Domains API. Not a finding.
        return warnings;
    }
    if status == 404 {
        warnings.push(format!(
            "Mailgun has no domain named {} in this region; check sender_domain and use_eu_endpoint",
            settings.domain
        ));
        return warnings;
    }
    if !(200..300).contains(&status) {
        return warnings;
    }

    let body = response.body();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&body) else {
        return warnings;
    };
    let state = map
        .get("domain")
        .and_then(Value::as_object)
        .and_then(|domain| domain.get("state"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|state| !state.is_empty());

    match state {
        Some("active") | None => {}
        Some("unverified") => warnings.push(format!(
            "the Mailgun domain {} is unverified; Mailgun restricts sending until its DNS records are verified",
            settings.domain
        )),
        Some("disabled") => warnings.push(format!(
            "the Mailgun domain {} is disabled; Mailgun will refuse every message sent through it",
            settings.domain
        )),
        Some(other) => warnings.push(format!(
            "the Mailgun domain {} is in state {other:?} rather than active",
            settings.domain
        )),
    }

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
        NotificationMediaUpdateType, NotificationSeverity, PluginNotificationApp,
        PluginNotificationApplicationUpdate, PluginNotificationDownload,
        PluginNotificationExternalIds, PluginNotificationFile, PluginNotificationHealth,
        PluginNotificationImport, PluginNotificationManualInteraction, PluginNotificationMediaFile,
        PluginNotificationMediaUpdate, PluginNotificationRelease, PluginNotificationTitle,
    };

    // -----------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------

    fn settings() -> Settings {
        Settings {
            api_key: "key-0123456789".to_string(),
            base_url: US_BASE_URL,
            domain: "mg.example.com".to_string(),
            from: "Scryer <scryer@mg.example.com>".to_string(),
            recipients: vec!["ops@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            tags: Vec::new(),
            send_html: true,
        }
    }

    /// The sparse shape the core actually sends today: a summary pair, the app
    /// block, and nothing else.
    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: Some("evt-1".to_string()),
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
            poster_url: Some("https://images.example.com/poster.jpg".to_string()),
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds::default(),
        }
    }

    /// Every optional block populated, which is the shape the contract can carry
    /// even though the dispatcher fills only part of it today.
    fn populated_request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            title: Some(series_title()),
            episode: Some(PluginNotificationEpisode {
                display: Some("1x01 - Pilot".to_string()),
                ..PluginNotificationEpisode::default()
            }),
            release: Some(PluginNotificationRelease {
                source_title: Some("Example.Show.S01E01.1080p.WEB-DL".to_string()),
                quality: Some("WEBDL-1080p".to_string()),
                release_group: Some("NTb".to_string()),
                indexer: Some("Example Indexer".to_string()),
                ..PluginNotificationRelease::default()
            }),
            download: Some(PluginNotificationDownload {
                client_name: Some("Weaver".to_string()),
                title: Some("Example.Show.S01E01".to_string()),
                status: Some("failed".to_string()),
                status_message: Some("No usable article".to_string()),
                size_bytes: Some(2_147_483_648),
                ..PluginNotificationDownload::default()
            }),
            import: Some(PluginNotificationImport {
                source_path: Some("/downloads/Example.Show.S01E01".to_string()),
                dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
                status: Some("imported".to_string()),
                ..PluginNotificationImport::default()
            }),
            health: Some(PluginNotificationHealth {
                code: Some("IndexerStatusCheck".to_string()),
                details: Some("Indexer unavailable".to_string()),
                ..PluginNotificationHealth::default()
            }),
            file: Some(PluginNotificationFile {
                primary_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
                media_updates: vec![PluginNotificationMediaUpdate {
                    path: "/media/TV/Example Show/S01E01.old.mkv".to_string(),
                    update_type: NotificationMediaUpdateType::Deleted,
                }],
            }),
            media_files: vec![PluginNotificationMediaFile {
                path: "/media/TV/Example Show/S01E01.mkv".to_string(),
                subtitle_languages: vec!["English".to_string()],
                ..PluginNotificationMediaFile::default()
            }],
            application_update: Some(PluginNotificationApplicationUpdate {
                current_version: Some("0.19.7".to_string()),
                target_version: Some("0.19.8".to_string()),
                ..PluginNotificationApplicationUpdate::default()
            }),
            manual_interaction: Some(PluginNotificationManualInteraction {
                reason: Some("Waiting for the operator".to_string()),
                ..PluginNotificationManualInteraction::default()
            }),
            ..request(event_type)
        }
    }

    fn params_of(
        req: &PluginNotificationRequest,
        settings: &Settings,
    ) -> (Vec<(String, String)>, Vec<String>) {
        build_params(req, settings)
    }

    fn value<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
        params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn values(params: &[(String, String)], key: &str) -> Vec<String> {
        params
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .collect()
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn no_warnings() -> Vec<String> {
        Vec::new()
    }

    // -----------------------------------------------------------------
    // Descriptor
    // -----------------------------------------------------------------

    #[test]
    fn descriptor_keeps_every_june_config_key_and_makes_recipients_a_tag_field() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("mailgun is a notification provider");
        };

        let by_key: BTreeMap<&str, &ConfigFieldDef> = notification
            .config_fields
            .iter()
            .map(|field| (field.key.as_str(), field))
            .collect();

        // The five June keys are a public contract and none of them may move.
        for key in [
            "api_key",
            "use_eu_endpoint",
            "from",
            "sender_domain",
            "recipients",
        ] {
            assert!(by_key.contains_key(key), "config key {key} disappeared");
        }
        assert_eq!(by_key["api_key"].field_type, ConfigFieldType::Password);
        assert_eq!(by_key["use_eu_endpoint"].field_type, ConfigFieldType::Bool);
        // `MailgunSettings.cs:40` models recipients as a tag list; the June port
        // made it a free string.
        assert_eq!(by_key["recipients"].field_type, ConfigFieldType::Tag);
        assert!(by_key["recipients"].required);
        assert!(by_key["api_key"].required);
        assert!(by_key["from"].required);
        assert!(by_key["sender_domain"].required);

        // Added, and therefore optional.
        for key in ["cc", "bcc", "tags", "send_html"] {
            assert!(by_key.contains_key(key), "expected the {key} field");
            assert!(!by_key[key].required, "{key} must not be required");
        }
    }

    #[test]
    fn descriptor_declares_both_regions_and_an_html_payload() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("mailgun is a notification provider");
        };
        assert_eq!(notification.allowed_hosts, vec![US_HOST, EU_HOST]);
        assert!(
            notification
                .capabilities
                .payload_formats
                .contains(&NotificationPayloadFormat::Html)
        );
        assert!(notification.capabilities.supports_rich_text);
        assert!(notification.capabilities.supports_test);
        assert!(!notification.capabilities.requires_host_process);
        assert!(!notification.capabilities.requires_host_filesystem);
    }

    // -----------------------------------------------------------------
    // Endpoint
    // -----------------------------------------------------------------

    #[test]
    fn the_endpoint_follows_the_region_flag() {
        let mut settings = settings();
        assert_eq!(
            settings.messages_url(),
            "https://api.mailgun.net/v3/mg.example.com/messages"
        );
        settings.base_url = EU_BASE_URL;
        assert_eq!(
            settings.messages_url(),
            "https://api.eu.mailgun.net/v3/mg.example.com/messages"
        );
        // The Domains API moved to v4; Sonarr's proxy pins v3 for everything.
        assert_eq!(
            settings.domain_url(),
            "https://api.eu.mailgun.net/v4/domains/mg.example.com"
        );
    }

    // -----------------------------------------------------------------
    // Address parsing and validation
    // -----------------------------------------------------------------

    #[test]
    fn addresses_are_parsed_the_way_mailgun_accepts_them() {
        assert_eq!(
            parse_address("ops@example.com").as_deref(),
            Some("ops@example.com")
        );
        assert_eq!(
            parse_address("  Ops Team <ops@example.com> ").as_deref(),
            Some("Ops Team <ops@example.com>")
        );
        assert_eq!(
            parse_address("<ops@example.com>").as_deref(),
            Some("ops@example.com")
        );
        // A quoted display name is unquoted rather than sent with the quotes.
        assert_eq!(
            parse_address("\"Ops\" <ops@example.com>").as_deref(),
            Some("Ops <ops@example.com>")
        );

        for bad in [
            "",
            "ops",
            "ops@",
            "@example.com",
            "ops@example",
            "ops@.com",
            "ops@example.com.",
            "ops example@example.com",
            "ops@example.com, other@example.com",
            "ops@example.com\r\nBcc: attacker@example.com",
            "Ops <ops@example.com",
        ] {
            assert!(
                parse_address(bad).is_none(),
                "{bad:?} must not parse as an address"
            );
        }
    }

    #[test]
    fn an_empty_recipient_list_is_a_configuration_error_not_a_delivery_failure() {
        // The June port answered this with `error_response(...)`, i.e. a failed
        // delivery (`lib.rs:95-97`).
        let error = validated_addresses("recipients", &[], true, false, &mut no_warnings())
            .expect_err("an empty recipient list cannot send");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("recipients"));
    }

    #[test]
    fn a_malformed_recipient_fails_a_test_and_only_warns_on_a_live_send() {
        let configured = vec!["ops@example.com".to_string(), "not-an-address".to_string()];

        let error = validated_addresses("recipients", &configured, true, true, &mut no_warnings())
            .expect_err("a connection test is strict");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("not-an-address"));

        let mut warnings = no_warnings();
        let addresses =
            validated_addresses("recipients", &configured, true, false, &mut warnings).unwrap();
        assert_eq!(addresses, vec!["ops@example.com".to_string()]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not-an-address"));
    }

    #[test]
    fn a_recipient_list_with_nothing_usable_is_an_error_even_on_a_live_send() {
        let configured = vec!["nope".to_string()];
        let error = validated_addresses("recipients", &configured, true, false, &mut no_warnings())
            .expect_err("there is nothing to degrade to");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn recipients_are_deduplicated_case_insensitively_on_the_address() {
        let configured = vec![
            "Ops <ops@example.com>".to_string(),
            "OPS@EXAMPLE.COM".to_string(),
            "other@example.com".to_string(),
        ];
        let addresses =
            validated_addresses("recipients", &configured, true, true, &mut no_warnings()).unwrap();
        assert_eq!(
            addresses,
            vec![
                "Ops <ops@example.com>".to_string(),
                "other@example.com".to_string()
            ]
        );
    }

    #[test]
    fn the_thousand_recipient_ceiling_keeps_the_visible_recipients() {
        let many: Vec<String> = (0..RECIPIENT_LIMIT)
            .map(|index| format!("user{index}@example.com"))
            .collect();
        let mut warnings = no_warnings();
        let (to, cc, bcc) = enforce_recipient_limit(
            many.clone(),
            vec!["cc@example.com".to_string()],
            vec!["bcc@example.com".to_string()],
            &mut warnings,
        );
        assert_eq!(to.len(), RECIPIENT_LIMIT);
        assert!(cc.is_empty());
        assert!(bcc.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(&RECIPIENT_LIMIT.to_string()));
    }

    #[test]
    fn the_sender_domain_is_normalised_and_a_path_is_refused() {
        let mut warnings = no_warnings();
        assert_eq!(
            validated_domain("  HTTPS://MG.Example.com/ ", true, &mut warnings).unwrap(),
            "mg.example.com"
        );
        assert!(warnings.is_empty());

        for bad in ["", "mg.example.com/messages", "user@mg.example.com"] {
            let error = validated_domain(bad, false, &mut no_warnings())
                .expect_err("{bad} must not be usable as a path segment");
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(error.public_message.contains("sender_domain"));
        }
    }

    #[test]
    fn an_implausible_sender_domain_fails_a_test_and_warns_on_a_live_send() {
        let error = validated_domain("localhost", true, &mut no_warnings())
            .expect_err("a connection test is strict");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);

        let mut warnings = no_warnings();
        assert_eq!(
            validated_domain("localhost", false, &mut warnings).unwrap(),
            "localhost"
        );
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn the_from_address_is_validated_which_sonarr_never_does() {
        // `MailgunSettings.cs:14` only requires it to be non-empty.
        assert_eq!(
            validated_from("Scryer <scryer@mg.example.com>").unwrap(),
            "Scryer <scryer@mg.example.com>"
        );
        let error = validated_from("scryer").expect_err("a bare word is not an address");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("from"));
    }

    #[test]
    fn tags_respect_mailguns_three_tag_and_128_character_limits() {
        let long = "t".repeat(TAG_CHARACTER_LIMIT + 1);
        let error = validated_tags(std::slice::from_ref(&long), true, &mut no_warnings())
            .expect_err("a connection test is strict");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);

        let mut warnings = no_warnings();
        assert!(
            validated_tags(&[long, "café".to_string()], false, &mut warnings)
                .unwrap()
                .is_empty()
        );
        assert_eq!(warnings.len(), 2, "both tags are unusable: {warnings:?}");

        let four: Vec<String> = ["a", "b", "c", "d"].iter().map(|t| t.to_string()).collect();
        assert!(validated_tags(&four, true, &mut no_warnings()).is_err());
        let mut warnings = no_warnings();
        let kept = validated_tags(&four, false, &mut warnings).unwrap();
        assert_eq!(kept.len(), TAG_LIMIT);
        assert_eq!(warnings.len(), 1);
    }

    // -----------------------------------------------------------------
    // Payload
    // -----------------------------------------------------------------

    #[test]
    fn the_sparse_request_still_produces_sonarrs_four_parameters() {
        let (params, warnings) = params_of(&request(NotificationEventType::Grab), &settings());
        assert!(warnings.is_empty(), "{warnings:?}");

        assert_eq!(
            value(&params, "from"),
            Some("Scryer <scryer@mg.example.com>")
        );
        assert_eq!(values(&params, "to"), vec!["ops@example.com".to_string()]);
        assert_eq!(value(&params, "subject"), Some("Grabbed: Example Show"));
        // The June body, verbatim, as the first paragraph.
        let text = value(&params, "text").expect("a text part");
        assert!(
            text.starts_with("Grabbed 'Example.Show.S01E01' for 'Example Show'."),
            "the summary must lead the body: {text}"
        );
        // Nothing but the event line is added when the core sent no blocks.
        assert!(text.contains("Event: grab"), "{text}");
        assert!(!text.contains("Quality:"), "{text}");
    }

    #[test]
    fn every_recipient_becomes_its_own_form_parameter() {
        let settings = Settings {
            recipients: vec!["a@example.com".to_string(), "b@example.com".to_string()],
            cc: vec!["c@example.com".to_string()],
            bcc: vec!["d@example.com".to_string()],
            ..settings()
        };
        let (params, _) = params_of(&request(NotificationEventType::Grab), &settings);
        assert_eq!(
            values(&params, "to"),
            vec!["a@example.com".to_string(), "b@example.com".to_string()]
        );
        assert_eq!(values(&params, "cc"), vec!["c@example.com".to_string()]);
        assert_eq!(values(&params, "bcc"), vec!["d@example.com".to_string()]);
    }

    #[test]
    fn tags_and_the_event_headers_travel_as_mailgun_options() {
        let settings = Settings {
            tags: vec!["scryer".to_string(), "grab".to_string()],
            ..settings()
        };
        let (params, _) = params_of(&request(NotificationEventType::Grab), &settings);
        assert_eq!(
            values(&params, "o:tag"),
            vec!["scryer".to_string(), "grab".to_string()]
        );
        assert_eq!(value(&params, "h:X-Scryer-Event-Type"), Some("grab"));
        assert_eq!(value(&params, "h:X-Scryer-Event-Id"), Some("evt-1"));
    }

    #[test]
    fn the_html_alternative_is_sent_alongside_the_text_and_can_be_turned_off() {
        let (params, _) = params_of(&populated_request(NotificationEventType::Grab), &settings());
        let html = value(&params, "html").expect("an html part");
        assert!(html.starts_with("<html>"), "{html}");
        assert!(html.contains("<td"), "the detail table is missing: {html}");
        assert!(html.contains("WEBDL-1080p"), "{html}");
        assert!(html.contains("Sent by Scryer 0.19.8."), "{html}");
        assert!(
            value(&params, "text").is_some(),
            "the text part is required"
        );

        let plain_only = Settings {
            send_html: false,
            ..settings()
        };
        let (params, _) = params_of(&request(NotificationEventType::Grab), &plain_only);
        assert!(value(&params, "html").is_none());
        assert!(value(&params, "text").is_some());
    }

    #[test]
    fn the_html_part_escapes_everything_it_renders() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_title = "Grabbed: <script>alert(1)</script>".to_string();
        req.summary_message = "a & b <b>bold</b>".to_string();

        let (params, _) = params_of(&req, &settings());
        let html = value(&params, "html").expect("an html part");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("a &amp; b"), "{html}");
        // The plain part is not escaped: it is not markup.
        assert!(
            value(&params, "text")
                .unwrap()
                .contains("a & b <b>bold</b>")
        );
    }

    #[test]
    fn the_poster_is_embedded_when_the_contract_carries_one() {
        let (params, _) = params_of(&populated_request(NotificationEventType::Grab), &settings());
        let html = value(&params, "html").expect("an html part");
        assert!(
            html.contains("src=\"https://images.example.com/poster.jpg\""),
            "{html}"
        );

        let (params, _) = params_of(&request(NotificationEventType::Grab), &settings());
        assert!(!value(&params, "html").unwrap().contains("<img"));
    }

    #[test]
    fn a_subject_is_always_present_and_never_carries_a_newline() {
        let mut req = request(NotificationEventType::Test);
        req.summary_title = "Test\r\nBcc: attacker@example.com".to_string();
        let mut warnings = no_warnings();
        let rendered = subject(&req, &mut warnings);
        assert!(
            !rendered.contains('\n') && !rendered.contains('\r'),
            "{rendered}"
        );

        req.summary_title = String::new();
        assert_eq!(subject(&req, &mut warnings), "Scryer");

        req.app.name = String::new();
        assert_eq!(subject(&req, &mut warnings), FALLBACK_SUBJECT);

        req.summary_title = "s".repeat(SUBJECT_CHARACTER_LIMIT + 50);
        let mut warnings = no_warnings();
        let rendered = subject(&req, &mut warnings);
        assert_eq!(rendered.chars().count(), SUBJECT_CHARACTER_LIMIT);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_request_with_no_summary_still_has_a_body_mailgun_will_accept() {
        let mut req = request(NotificationEventType::Test);
        req.summary_message = String::new();
        let (params, _) = params_of(&req, &settings());
        let text = value(&params, "text").expect("a text part");
        assert!(!text.trim().is_empty());
    }

    #[test]
    fn an_over_long_value_is_trimmed_with_a_warning() {
        let mut req = request(NotificationEventType::Grab);
        req.release = Some(PluginNotificationRelease {
            source_title: Some("x".repeat(VALUE_CHARACTER_LIMIT + 10)),
            ..PluginNotificationRelease::default()
        });
        let (params, warnings) = params_of(&req, &settings());
        assert!(
            warnings.iter().any(|warning| warning.contains("Release")),
            "{warnings:?}"
        );
        assert!(value(&params, "text").unwrap().contains('…'));
    }

    // -----------------------------------------------------------------
    // Per-event rendering
    // -----------------------------------------------------------------

    #[test]
    fn a_grab_renders_the_release_facts() {
        let lines = detail_lines(
            &populated_request(NotificationEventType::Grab),
            &mut no_warnings(),
        );
        let rendered: BTreeMap<&str, String> = lines.iter().cloned().collect();
        assert_eq!(rendered["Episode"], "1x01 - Pilot");
        assert_eq!(rendered["Quality"], "WEBDL-1080p");
        assert_eq!(rendered["Release"], "Example.Show.S01E01.1080p.WEB-DL");
        assert_eq!(rendered["Release Group"], "NTb");
        assert_eq!(rendered["Indexer"], "Example Indexer");
        assert_eq!(rendered["Size"], "2 GB");
        assert_eq!(rendered["Client"], "Weaver");
        assert_eq!(rendered["Title"], "Example Show (2024)");
    }

    /// `NotificationEventType::Download` carries a *failed* download, never an
    /// import (dispatcher.rs:34,418-448 on release-0.19.8).
    #[test]
    fn a_download_event_renders_a_failure_and_never_a_destination() {
        let lines = detail_lines(
            &populated_request(NotificationEventType::Download),
            &mut no_warnings(),
        );
        let rendered: BTreeMap<&str, String> = lines.iter().cloned().collect();
        assert_eq!(rendered["Status"], "No usable article");
        assert_eq!(rendered["Client"], "Weaver");
        assert!(!rendered.contains_key("Destination"));
    }

    #[test]
    fn an_import_renders_its_destination() {
        let lines = detail_lines(
            &populated_request(NotificationEventType::ImportComplete),
            &mut no_warnings(),
        );
        let rendered: BTreeMap<&str, String> = lines.iter().cloned().collect();
        assert_eq!(rendered["Destination"], "/media/TV/Example Show/S01E01.mkv");
    }

    #[test]
    fn a_delete_names_the_deleted_file() {
        let lines = detail_lines(
            &populated_request(NotificationEventType::FileDeletedForUpgrade),
            &mut no_warnings(),
        );
        let rendered: BTreeMap<&str, String> = lines.iter().cloned().collect();
        assert_eq!(rendered["File"], "/media/TV/Example Show/S01E01.old.mkv");
    }

    #[test]
    fn health_and_update_events_render_without_a_title_block() {
        for event in [
            NotificationEventType::HealthIssue,
            NotificationEventType::HealthRestored,
            NotificationEventType::ApplicationUpdate,
            NotificationEventType::ManualInteractionRequired,
            NotificationEventType::SubtitleDownloaded,
            NotificationEventType::MediaRequestSubmitted,
            NotificationEventType::Test,
        ] {
            // Nothing may panic or fail on an event this channel does not
            // special-case beyond its own arm.
            let mut req = populated_request(event);
            req.title = None;
            let (params, _) = params_of(&req, &settings());
            assert!(value(&params, "text").is_some(), "{event:?}");
        }
    }

    // -----------------------------------------------------------------
    // Delivery classification
    // -----------------------------------------------------------------

    fn classify(status: u16, body: &str) -> PluginResult<PluginNotificationResponse> {
        classify_response(
            status,
            &headers(&[]),
            body.as_bytes(),
            &settings(),
            no_warnings(),
        )
    }

    #[test]
    fn a_queued_message_records_mailguns_id_for_every_recipient() {
        let settings = Settings {
            recipients: vec!["a@example.com".to_string()],
            cc: vec!["c@example.com".to_string()],
            ..settings()
        };
        let result = classify_response(
            200,
            &headers(&[]),
            br#"{"id":"<20260902.1@mg.example.com>","message":"Queued. Thank you."}"#,
            &settings,
            no_warnings(),
        );
        let PluginResult::Ok(response) = result else {
            panic!("a 200 is a delivery");
        };
        assert!(response.success);
        assert_eq!(
            response.delivery_id.as_deref(),
            Some("<20260902.1@mg.example.com>")
        );
        // Mailgun queues; it does not report per-recipient delivery here.
        assert_eq!(response.provider_status.as_deref(), Some("queued"));
        assert_eq!(response.target_results.len(), 2);
        assert!(response.target_results.iter().all(|target| target.success));
        assert_eq!(response.target_results[0].target, "a@example.com");
        assert_eq!(response.target_results[1].target, "c@example.com");
        assert!(response.warnings.is_empty());
    }

    #[test]
    fn a_2xx_that_is_not_the_mailgun_api_is_delivered_with_a_warning() {
        let PluginResult::Ok(response) = classify(200, "<html>hello</html>") else {
            panic!("a 200 is a delivery");
        };
        assert!(response.success);
        assert_eq!(response.warnings.len(), 1);
        assert!(response.warnings[0].contains("did not answer like the Mailgun API"));
    }

    #[test]
    fn a_401_names_the_api_key() {
        // `MailgunProxy.cs:35-38`: "Unauthorised - ApiKey is invalid".
        let PluginResult::Err(error) = classify(401, r#"{"message":"Invalid private key"}"#) else {
            panic!("a 401 is a configuration problem, not a delivery outcome");
        };
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("api_key"));
        assert!(error.public_message.contains("use_eu_endpoint"));
        assert_eq!(
            error.debug_message.as_deref(),
            Some("HTTP 401: Invalid private key")
        );
    }

    #[test]
    fn a_sandbox_403_names_the_recipients_and_the_authorized_recipient_rule() {
        let PluginResult::Err(error) = classify(
            403,
            r#"{"message":"Sandbox subdomains are for test purposes only. Please add your own domain or add the address to authorized recipients in Account Settings."}"#,
        ) else {
            panic!("a 403 is a configuration problem");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("Authorized Recipients"));
        assert!(error.public_message.contains("recipients"));
    }

    #[test]
    fn a_plain_403_is_a_permission_problem_on_the_key() {
        let PluginResult::Err(error) = classify(403, r#"{"message":"Forbidden"}"#) else {
            panic!("a 403 is a configuration problem");
        };
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("api_key"));
        assert!(error.public_message.contains("sender_domain"));
    }

    #[test]
    fn a_404_names_the_domain_and_the_region() {
        let PluginResult::Err(error) =
            classify(404, r#"{"message":"Domain not found: mg.example.com"}"#)
        else {
            panic!("a 404 is a configuration problem");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("sender_domain"));
        assert!(error.public_message.contains("use_eu_endpoint"));
    }

    #[test]
    fn a_400_is_attributed_to_the_setting_mailgun_named() {
        let cases: [(&str, PluginErrorCode, &str); 5] = [
            (
                r#"{"message":"'from' parameter is missing"}"#,
                PluginErrorCode::InvalidConfig,
                "from",
            ),
            (
                r#"{"message":"'to' parameter is not a valid address"}"#,
                PluginErrorCode::InvalidConfig,
                "recipients",
            ),
            (
                r#"{"message":"tag is too long"}"#,
                PluginErrorCode::InvalidConfig,
                "tags",
            ),
            (
                r#"{"message":"the domain is not verified"}"#,
                PluginErrorCode::InvalidConfig,
                "sender_domain",
            ),
            (
                r#"{"message":"something else entirely"}"#,
                PluginErrorCode::Permanent,
                "this plugin built",
            ),
        ];

        for (body, code, needle) in cases {
            let PluginResult::Err(error) = classify(400, body) else {
                panic!("a 400 is never a delivery outcome: {body}");
            };
            assert_eq!(error.code, code, "{body}");
            assert!(
                error.public_message.contains(needle),
                "{body} -> {}",
                error.public_message
            );
        }
    }

    #[test]
    fn a_402_and_a_5xx_are_delivery_failures_not_settings_errors() {
        for status in [402u16, 500, 502, 503] {
            let PluginResult::Ok(response) = classify(status, r#"{"message":"nope"}"#) else {
                panic!("HTTP {status} is a delivery outcome");
            };
            assert!(!response.success);
            assert_eq!(
                response.provider_status.as_deref(),
                Some(format!("http_{status}").as_str())
            );
            assert_eq!(response.target_results.len(), 1);
            assert!(!response.target_results[0].success);
        }
    }

    #[test]
    fn a_413_is_this_plugins_fault() {
        let PluginResult::Err(error) = classify(413, r#"{"message":"Message too large"}"#) else {
            panic!("a 413 is not a delivery outcome");
        };
        assert_eq!(error.code, PluginErrorCode::Permanent);
    }

    #[test]
    fn a_429_carries_the_retry_delay_from_either_header() {
        let result = classify_response(
            429,
            &headers(&[("Retry-After", "45")]),
            br#"{"message":"Too many requests"}"#,
            &settings(),
            no_warnings(),
        );
        let PluginResult::Ok(response) = result else {
            panic!("a 429 is a delivery outcome");
        };
        assert!(!response.success);
        assert_eq!(response.retry_after_seconds, Some(45));
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));

        // `X-RateLimit-Reset` is Unix *milliseconds*, so it has to become a
        // delay rather than being passed through.
        let future_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 120)
            * 1000;
        let result = classify_response(
            429,
            &headers(&[("x-ratelimit-reset", &future_ms.to_string())]),
            br#"{"message":"Too many requests"}"#,
            &settings(),
            no_warnings(),
        );
        let PluginResult::Ok(response) = result else {
            panic!("a 429 is a delivery outcome");
        };
        let seconds = response.retry_after_seconds.expect("a retry delay");
        assert!((115..=120).contains(&seconds), "got {seconds}");
    }

    #[test]
    fn an_unknown_4xx_still_names_a_setting_rather_than_failing_silently() {
        let PluginResult::Err(error) = classify(418, "teapot") else {
            panic!("an unrecognised 4xx is a configuration problem");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("418"));
    }

    #[test]
    fn size_rounding_matches_the_rest_of_the_fleet() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
        assert_eq!(format_bytes(1_610_612_736), "1.5 GB");
    }
}
