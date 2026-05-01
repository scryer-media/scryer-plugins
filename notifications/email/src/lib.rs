use base64::{engine::general_purpose::STANDARD, Engine as _};
use extism_pdk::*;
use scryer_plugin_sdk::{
    current_sdk_constraint, ConfigFieldDef, ConfigFieldOption, ConfigFieldType,
    NotificationCapabilities, NotificationDeliveryMode, NotificationDescriptor,
    NotificationPayloadFormat, PluginDescriptor, PluginError, PluginErrorCode,
    PluginNotificationRequest, PluginNotificationResponse, PluginNotificationTargetResult,
    PluginResult, ProviderDescriptor, SocketCloseRequest, SocketOpenRequest, SocketPermission,
    SocketReadRequest, SocketStartTlsRequest, SocketTlsMode, SocketWriteRequest, SDK_VERSION,
};
use wasm_smtp::{
    AuthError, IoError, ProtocolError, SmtpClient as WasmSmtpClient, SmtpError, SmtpOp,
    StartTlsCapable, Transport,
};

const DEFAULT_HELLO_NAME: &str = "scryer.local";
const SOCKET_TIMEOUT_MS: u64 = 30_000;
const CONNECT_TIMEOUT_MS: u64 = 10_000;
const SMTP_READ_BYTES: usize = 4096;
const MAX_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "email".to_string(),
        name: "Email".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![SocketPermission {
            host_pattern: "${smtp_host}".to_string(),
            ports: vec![25, 465, 587],
            tls_modes: vec![
                SocketTlsMode::Plain,
                SocketTlsMode::Starttls,
                SocketTlsMode::Tls,
            ],
        }],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: "email".to_string(),
            provider_aliases: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                supports_rich_text: false,
                supports_images: false,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![NotificationDeliveryMode::Email],
                payload_formats: vec![NotificationPayloadFormat::PlainText],
                supported_events: vec![],
                event_options: Default::default(),
            },
            config_fields: vec![
                field(
                    "smtp_host",
                    "SMTP Host",
                    ConfigFieldType::String,
                    true,
                    None,
                ),
                field(
                    "smtp_port",
                    "SMTP Port",
                    ConfigFieldType::Number,
                    true,
                    Some("587"),
                ),
                ConfigFieldDef {
                    key: "security".to_string(),
                    label: "Security".to_string(),
                    field_type: ConfigFieldType::Select,
                    required: true,
                    default_value: Some("starttls".to_string()),
                    value_source: Default::default(),
                    host_binding: None,
                    options: vec![
                        option("plain", "Plain"),
                        option("starttls", "STARTTLS"),
                        option("tls", "TLS"),
                    ],
                    help_text: Some(
                        "SMTP transport security: plain, STARTTLS, or implicit TLS.".to_string(),
                    ),
                },
                field(
                    "from_address",
                    "From Address",
                    ConfigFieldType::String,
                    true,
                    None,
                ),
                field(
                    "to_addresses",
                    "To Addresses",
                    ConfigFieldType::Multiline,
                    true,
                    None,
                ),
                field("username", "Username", ConfigFieldType::String, false, None),
                field(
                    "password",
                    "Password",
                    ConfigFieldType::Password,
                    false,
                    None,
                ),
                field(
                    "subject_prefix",
                    "Subject Prefix",
                    ConfigFieldType::String,
                    false,
                    None,
                ),
                field("reply_to", "Reply-To", ConfigFieldType::String, false, None),
                ConfigFieldDef {
                    help_text: Some(
                        "Hostname sent in the SMTP EHLO greeting. Leave blank to use the default unless your mail server requires a specific fully qualified domain name."
                            .to_string(),
                    ),
                    ..field(
                        "hello_name",
                        "EHLO Name",
                        ConfigFieldType::String,
                        false,
                        None,
                    )
                },
            ],
        }),
    }
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: Default::default(),
        host_binding: None,
        options: vec![],
        help_text: None,
    }
}

fn option(value: &str, label: &str) -> ConfigFieldOption {
    ConfigFieldOption {
        value: value.to_string(),
        label: label.to_string(),
    }
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let request: PluginNotificationRequest = serde_json::from_str(&input)?;

    let result =
        match EmailConfig::from_host_config().and_then(|config| send_email(&config, &request)) {
            Ok(delivery) => PluginResult::Ok(PluginNotificationResponse {
                success: true,
                error: None,
                delivery_id: None,
                provider_status: Some("smtp_accepted".to_string()),
                retry_after_seconds: None,
                warnings: delivery.warnings,
                target_results: delivery.target_results,
            }),
            Err(error) => PluginResult::Err(error.into_plugin_error()),
        };

    Ok(serde_json::to_string(&result)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecurityMode {
    Plain,
    StartTls,
    Tls,
}

impl SecurityMode {
    fn parse(value: &str) -> Result<Self, EmailFailure> {
        match value.trim().to_ascii_lowercase().as_str() {
            "plain" => Ok(Self::Plain),
            "starttls" => Ok(Self::StartTls),
            "tls" => Ok(Self::Tls),
            value => Err(EmailFailure::invalid_config(format!(
                "unsupported email security mode {value}"
            ))),
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Plain => 25,
            Self::StartTls => 587,
            Self::Tls => 465,
        }
    }

    fn socket_tls_mode(self) -> SocketTlsMode {
        match self {
            Self::Plain => SocketTlsMode::Plain,
            Self::StartTls => SocketTlsMode::Starttls,
            Self::Tls => SocketTlsMode::Tls,
        }
    }
}

#[derive(Debug, Clone)]
struct EmailConfig {
    smtp_host: String,
    smtp_port: u16,
    security: SecurityMode,
    from_address: String,
    to_addresses: Vec<String>,
    username: Option<String>,
    password: Option<String>,
    subject_prefix: Option<String>,
    reply_to: Option<String>,
    hello_name: String,
}

impl EmailConfig {
    fn from_host_config() -> Result<Self, EmailFailure> {
        Self::from_values(|key| config::get(key).ok().flatten())
    }

    fn from_values(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, EmailFailure> {
        let security = SecurityMode::parse(
            get("security")
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("starttls"),
        )?;
        let smtp_port = parse_port(get("smtp_port").as_deref(), security)?;
        let smtp_host = required_config(&mut get, "smtp_host")?;
        let from_address = normalize_address(&required_config(&mut get, "from_address")?)?;
        let to_addresses = parse_recipients(&required_config(&mut get, "to_addresses")?)?;
        let username = optional_config(&mut get, "username");
        let password = optional_config(&mut get, "password");

        if username.is_some() != password.is_some() {
            return Err(EmailFailure::invalid_config(
                "username and password must be configured together",
            ));
        }

        Ok(Self {
            smtp_host: smtp_host.trim().to_string(),
            smtp_port,
            security,
            from_address,
            to_addresses,
            username,
            password,
            subject_prefix: optional_config(&mut get, "subject_prefix"),
            reply_to: optional_config(&mut get, "reply_to")
                .map(|value| normalize_address(&value))
                .transpose()?,
            hello_name: optional_config(&mut get, "hello_name")
                .unwrap_or_else(|| DEFAULT_HELLO_NAME.to_string()),
        })
    }
}

fn required_config(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &str,
) -> Result<String, EmailFailure> {
    get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| EmailFailure::invalid_config(format!("{key} is required")))
}

fn optional_config(get: &mut impl FnMut(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_port(value: Option<&str>, security: SecurityMode) -> Result<u16, EmailFailure> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(security.default_port());
    };
    let port = value.parse::<u16>().map_err(|_| {
        EmailFailure::invalid_config(format!("smtp_port must be a valid port: {value}"))
    })?;
    if port == 0 {
        Ok(security.default_port())
    } else {
        Ok(port)
    }
}

fn parse_recipients(value: &str) -> Result<Vec<String>, EmailFailure> {
    let recipients = value
        .split([',', '\n'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_address)
        .collect::<Result<Vec<_>, _>>()?;

    if recipients.is_empty() {
        Err(EmailFailure::invalid_config(
            "to_addresses must include at least one recipient",
        ))
    } else {
        Ok(recipients)
    }
}

fn normalize_address(value: &str) -> Result<String, EmailFailure> {
    let value = value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if value.is_empty()
        || value.contains(['\r', '\n'])
        || value.contains(' ')
        || !value.contains('@')
    {
        return Err(EmailFailure::invalid_config(format!(
            "invalid email address: {value}"
        )));
    }
    Ok(value.to_string())
}

#[derive(Debug)]
struct SmtpDelivery {
    warnings: Vec<String>,
    target_results: Vec<PluginNotificationTargetResult>,
}

fn send_email(
    config: &EmailConfig,
    request: &PluginNotificationRequest,
) -> Result<SmtpDelivery, EmailFailure> {
    let message = build_message(config, request, &config.to_addresses);
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(EmailFailure::new(
            PluginErrorCode::InvalidConfig,
            format!("email message exceeds the {MAX_MESSAGE_BYTES} byte limit"),
        ));
    }

    let recipients = config
        .to_addresses
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let transport = ScryerSocketTransport::open(config)?;

    futures::executor::block_on(async {
        let ehlo_name = sanitize_ehlo_name(&config.hello_name);
        let mut client = match config.security {
            SecurityMode::StartTls => {
                WasmSmtpClient::connect_starttls(transport, &ehlo_name).await?
            }
            SecurityMode::Plain | SecurityMode::Tls => {
                WasmSmtpClient::connect(transport, &ehlo_name).await?
            }
        };

        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            client.login(username, password).await?;
        }

        client
            .send_mail(&config.from_address, &recipients, &message)
            .await?;
        client.quit().await
    })
    .map_err(smtp_failure)?;

    Ok(SmtpDelivery {
        warnings: Vec::new(),
        target_results: config
            .to_addresses
            .iter()
            .map(|recipient| PluginNotificationTargetResult {
                target: recipient.clone(),
                success: true,
                status: Some("accepted".to_string()),
                error: None,
            })
            .collect(),
    })
}

struct ScryerSocketTransport {
    handle: u32,
    host: String,
}

impl ScryerSocketTransport {
    fn open(config: &EmailConfig) -> Result<Self, EmailFailure> {
        let response = scryer_plugin_sdk::net::socket_open(SocketOpenRequest {
            host: config.smtp_host.clone(),
            port: config.smtp_port,
            tls_mode: config.security.socket_tls_mode(),
            connect_timeout_ms: Some(CONNECT_TIMEOUT_MS),
            read_timeout_ms: Some(SOCKET_TIMEOUT_MS),
            write_timeout_ms: Some(SOCKET_TIMEOUT_MS),
        })
        .map_err(socket_failure)?;

        Ok(Self {
            handle: response.handle,
            host: config.smtp_host.clone(),
        })
    }
}

impl Transport for ScryerSocketTransport {
    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, IoError> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let response = scryer_plugin_sdk::net::socket_read(SocketReadRequest {
            handle: self.handle,
            max_bytes: buffer.len().min(SMTP_READ_BYTES),
        })
        .map_err(socket_io_error)?;
        let data = STANDARD
            .decode(response.data_base64)
            .map_err(|error| IoError::new(format!("failed to decode socket read: {error}")))?;
        let bytes_read = data.len().min(buffer.len());
        buffer[..bytes_read].copy_from_slice(&data[..bytes_read]);
        if bytes_read == 0 && response.eof {
            return Ok(0);
        }
        Ok(bytes_read)
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<(), IoError> {
        for chunk in data.chunks(32 * 1024) {
            scryer_plugin_sdk::net::socket_write(SocketWriteRequest {
                handle: self.handle,
                data_base64: STANDARD.encode(chunk),
            })
            .map_err(socket_io_error)?;
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<(), IoError> {
        scryer_plugin_sdk::net::socket_close(SocketCloseRequest {
            handle: self.handle,
        })
        .map_err(socket_io_error)?;
        Ok(())
    }
}

impl StartTlsCapable for ScryerSocketTransport {
    async fn upgrade_to_tls(&mut self) -> Result<(), IoError> {
        scryer_plugin_sdk::net::socket_starttls(SocketStartTlsRequest {
            handle: self.handle,
            host: self.host.clone(),
        })
        .map_err(socket_io_error)?;
        Ok(())
    }
}

impl Drop for ScryerSocketTransport {
    fn drop(&mut self) {
        let _ = scryer_plugin_sdk::net::socket_close(SocketCloseRequest {
            handle: self.handle,
        });
    }
}

fn socket_failure(error: scryer_plugin_sdk::SocketError) -> EmailFailure {
    let code = match error.code {
        scryer_plugin_sdk::SocketErrorCode::PermissionDenied => PluginErrorCode::InvalidConfig,
        scryer_plugin_sdk::SocketErrorCode::DnsFailed
        | scryer_plugin_sdk::SocketErrorCode::ConnectTimeout
        | scryer_plugin_sdk::SocketErrorCode::IoFailed
        | scryer_plugin_sdk::SocketErrorCode::RemoteClosed => PluginErrorCode::UpstreamUnavailable,
        scryer_plugin_sdk::SocketErrorCode::TlsVerificationFailed => PluginErrorCode::Permanent,
        scryer_plugin_sdk::SocketErrorCode::StartTlsFailed => PluginErrorCode::Unsupported,
        scryer_plugin_sdk::SocketErrorCode::AuthFailed => PluginErrorCode::AuthFailed,
        scryer_plugin_sdk::SocketErrorCode::ProtocolError => PluginErrorCode::Permanent,
        scryer_plugin_sdk::SocketErrorCode::Unsupported => PluginErrorCode::Unsupported,
    };
    EmailFailure::new(code, error.message)
}

fn socket_io_error(error: scryer_plugin_sdk::SocketError) -> IoError {
    IoError::new(format!("{:?}: {}", error.code, error.message))
}

fn smtp_failure(error: SmtpError) -> EmailFailure {
    let code = match &error {
        SmtpError::Auth(AuthError::Rejected { .. }) => PluginErrorCode::AuthFailed,
        SmtpError::Auth(AuthError::UnsupportedMechanism) => PluginErrorCode::InvalidConfig,
        SmtpError::Auth(_) => PluginErrorCode::AuthFailed,
        SmtpError::InvalidInput(_) => PluginErrorCode::InvalidConfig,
        SmtpError::Io(_) => PluginErrorCode::UpstreamUnavailable,
        SmtpError::Protocol(ProtocolError::ExtensionUnavailable { name })
            if *name == "STARTTLS" =>
        {
            PluginErrorCode::Unsupported
        }
        SmtpError::Protocol(ProtocolError::UnexpectedCode { actual, during, .. })
            if (400..500).contains(actual) =>
        {
            PluginErrorCode::Temporary
        }
        SmtpError::Protocol(ProtocolError::UnexpectedCode { during, .. })
            if matches!(*during, SmtpOp::MailFrom | SmtpOp::RcptTo) =>
        {
            PluginErrorCode::InvalidConfig
        }
        SmtpError::Protocol(ProtocolError::UnexpectedCode { during, .. })
            if matches!(*during, SmtpOp::StartTls) =>
        {
            PluginErrorCode::Unsupported
        }
        SmtpError::Protocol(_) => PluginErrorCode::Permanent,
    };
    EmailFailure::new(code, error.to_string())
}

#[derive(Debug, Clone)]
struct EmailFailure {
    code: PluginErrorCode,
    public_message: String,
    debug_message: Option<String>,
    retry_after_seconds: Option<i64>,
}

impl EmailFailure {
    fn new(code: PluginErrorCode, public_message: impl Into<String>) -> Self {
        Self {
            code,
            public_message: public_message.into(),
            debug_message: None,
            retry_after_seconds: None,
        }
    }

    fn invalid_config(public_message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::InvalidConfig, public_message)
    }

    fn into_plugin_error(self) -> PluginError {
        PluginError {
            code: self.code,
            public_message: self.public_message,
            debug_message: self.debug_message,
            retry_after_seconds: self.retry_after_seconds,
        }
    }
}

fn build_message(
    config: &EmailConfig,
    request: &PluginNotificationRequest,
    recipients: &[String],
) -> String {
    let subject = build_subject(config, request);
    let body = build_body(request);
    let mut message = String::new();

    push_header(&mut message, "From", &config.from_address);
    push_header(&mut message, "To", &recipients.join(", "));
    if let Some(reply_to) = &config.reply_to {
        push_header(&mut message, "Reply-To", reply_to);
    }
    push_header(&mut message, "Subject", &subject);
    push_header(&mut message, "MIME-Version", "1.0");
    push_header(&mut message, "Content-Type", "text/plain; charset=utf-8");
    push_header(&mut message, "Content-Transfer-Encoding", "8bit");
    push_header(
        &mut message,
        "X-Scryer-Event-Type",
        request.event_type.as_str(),
    );
    message.push_str("\r\n");
    message.push_str(&crlf_normalize(&body));
    if !message.ends_with("\r\n") {
        message.push_str("\r\n");
    }
    message
}

fn push_header(message: &mut String, name: &str, value: &str) {
    message.push_str(name);
    message.push_str(": ");
    message.push_str(&sanitize_header_value(value));
    message.push_str("\r\n");
}

fn build_subject(config: &EmailConfig, request: &PluginNotificationRequest) -> String {
    let title = if request.summary_title.trim().is_empty() {
        "Scryer Notification"
    } else {
        request.summary_title.trim()
    };
    match &config.subject_prefix {
        Some(prefix) => format!("{} {}", prefix.trim(), title),
        None => title.to_string(),
    }
}

fn build_body(request: &PluginNotificationRequest) -> String {
    let mut lines = vec![request.summary_message.trim().to_string()];
    lines.push(String::new());
    lines.push(format!("Event: {}", request.event_type.as_str()));
    if let Some(title) = &request.title {
        lines.push(format!("Title: {}", title.name));
    }
    if let Some(event_id) = &request.event_id {
        lines.push(format!("Event ID: {event_id}"));
    }
    if request.is_test {
        lines.push("This is a test notification from Scryer.".to_string());
    }
    lines.join("\n")
}

fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect::<String>()
}

fn sanitize_ehlo_name(value: &str) -> String {
    let sanitized = value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
        .collect::<String>();
    if sanitized.is_empty() {
        DEFAULT_HELLO_NAME.to_string()
    } else {
        sanitized
    }
}

fn crlf_normalize(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .collect::<Vec<_>>()
        .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        NotificationEventType, PluginNotificationApp, PluginNotificationExternalIds,
        PluginNotificationTitle,
    };

    #[test]
    fn parses_security_default_ports() {
        assert_eq!(SecurityMode::Plain.default_port(), 25);
        assert_eq!(SecurityMode::StartTls.default_port(), 587);
        assert_eq!(SecurityMode::Tls.default_port(), 465);
    }

    #[test]
    fn parses_recipients_from_commas_and_newlines() {
        let recipients = parse_recipients("a@example.com, b@example.com\nc@example.com").unwrap();
        assert_eq!(
            recipients,
            vec!["a@example.com", "b@example.com", "c@example.com"]
        );
    }

    #[test]
    fn rejects_header_injection_addresses() {
        assert!(normalize_address("bad@example.com\r\nCc: other@example.com").is_err());
    }

    #[test]
    fn renders_plaintext_message_headers_and_body() {
        let config = test_config();
        let request = test_request();
        let message = build_message(&config, &request, &config.to_addresses);

        assert!(message.contains("From: scryer@example.com\r\n"));
        assert!(message.contains("To: ops@example.com\r\n"));
        assert!(message.contains("Subject: [Scryer] Test Notification\r\n"));
        assert!(message.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(message.contains("This is a test."));
        assert!(message.ends_with("\r\n"));
    }

    #[test]
    fn normalizes_message_body_to_crlf() {
        assert_eq!(crlf_normalize("hello\n.world"), "hello\r\n.world");
    }

    #[test]
    fn config_defaults_port_by_security_mode() {
        let config = EmailConfig::from_values(|key| match key {
            "smtp_host" => Some("smtp.example.com".to_string()),
            "smtp_port" => Some("0".to_string()),
            "security" => Some("tls".to_string()),
            "from_address" => Some("scryer@example.com".to_string()),
            "to_addresses" => Some("ops@example.com".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.smtp_port, 465);
    }

    fn test_config() -> EmailConfig {
        EmailConfig {
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            security: SecurityMode::StartTls,
            from_address: "scryer@example.com".to_string(),
            to_addresses: vec!["ops@example.com".to_string()],
            username: None,
            password: None,
            subject_prefix: Some("[Scryer]".to_string()),
            reply_to: None,
            hello_name: DEFAULT_HELLO_NAME.to_string(),
        }
    }

    fn test_request() -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type: NotificationEventType::Test,
            event_id: Some("evt-1".to_string()),
            occurred_at: Some("2026-04-29T12:00:00Z".to_string()),
            correlation_id: None,
            actor: None,
            severity: None,
            is_test: true,
            summary_title: "Test Notification".to_string(),
            summary_message: "This is a test.".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "test".to_string(),
            },
            title: Some(PluginNotificationTitle {
                id: None,
                name: "Example Show".to_string(),
                facet: "tv".to_string(),
                year: Some(2026),
                slug: None,
                path: None,
                overview: None,
                sort_title: None,
                banner_url: None,
                background_url: None,
                poster_url: None,
                genres: Vec::new(),
                tags: Vec::new(),
                aliases: Vec::new(),
                original_language: None,
                original_country: None,
                external_ids: PluginNotificationExternalIds::default(),
            }),
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
        }
    }
}
