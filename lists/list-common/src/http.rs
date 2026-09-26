//! The one HTTP seam list plugins go through.
//!
//! Production code sends through [`HostHttp`], a single host-policy-checked
//! attempt over `scryer:runtime/host`. Unit tests swap in the recorded double
//! from the `testing` feature, so handler logic runs natively against
//! synthetic responses.

use std::future::Future;

use scryer_plugin_pdk::runtime::{self, HostError};
use scryer_plugin_sdk::PluginError;
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};

use crate::error::{invalid_config, permanent, plugin_error, unavailable};

pub trait ListHttp {
    fn send(
        &self,
        request: PluginHttpRequest,
    ) -> impl Future<Output = Result<PluginHttpResponse, PluginError>>;
}

/// Sends through the host's runtime HTTP capability.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostHttp;

impl ListHttp for HostHttp {
    async fn send(&self, request: PluginHttpRequest) -> Result<PluginHttpResponse, PluginError> {
        let host = host_of(&request.url).unwrap_or_default();
        runtime::http(request)
            .await
            .map_err(|error| transport_error(error, &host))
    }
}

/// Map a transport failure (no HTTP status at all) onto a plugin error.
pub fn transport_error(error: HostError, host: &str) -> PluginError {
    match error {
        HostError::ForbiddenOrigin => {
            invalid_config(format!("Scryer does not allow this plugin to reach {host}"))
        }
        HostError::InvalidRequest => invalid_config("the list URL is not a valid request"),
        HostError::ResponseTooLarge => permanent("the provider response is too large"),
        HostError::Cancelled => plugin_error(
            scryer_plugin_sdk::PluginErrorCode::Temporary,
            "the request was cancelled",
        ),
        HostError::Timeout | HostError::Capacity | HostError::Transport => {
            unavailable(format!("could not reach {host}: {error}"))
        }
    }
}

/// A GET request with the headers every list plugin sends.
pub fn get(url: impl Into<String>, user_agent: &str, accept: &str) -> PluginHttpRequest {
    let mut request = PluginHttpRequest {
        url: url.into(),
        method: Some("GET".to_string()),
        headers: Default::default(),
        body: Vec::new(),
    };
    request
        .headers
        .insert("User-Agent".to_string(), user_agent.to_string());
    request
        .headers
        .insert("Accept".to_string(), accept.to_string());
    request
}

/// Case-insensitive response header lookup.
pub fn header<'a>(response: &'a PluginHttpResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

pub fn body_text(response: &PluginHttpResponse) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

/// Decode a JSON body; a body that is not JSON is a permanent failure,
/// because retrying the same URL returns the same document.
pub fn json_body(response: &PluginHttpResponse) -> Result<serde_json::Value, PluginError> {
    serde_json::from_slice(&response.body)
        .map_err(|_| permanent("the provider response is not valid JSON"))
}

/// The lowercase host of an absolute http(s) URL.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Percent-encode one query or path component.
pub fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_handles_ports_userinfo_and_ipv6() {
        assert_eq!(
            host_of("https://Feeds.Example.test/a?b").as_deref(),
            Some("feeds.example.test")
        );
        assert_eq!(
            host_of("http://user@lists.example.test:8080/x").as_deref(),
            Some("lists.example.test")
        );
        assert_eq!(host_of("http://[::1]:9000/feed").as_deref(), Some("::1"));
        assert_eq!(host_of("ftp://example.test/"), None);
        assert_eq!(host_of("https:///path"), None);
    }

    #[test]
    fn encode_component_escapes_reserved_bytes() {
        assert_eq!(encode_component("a b/c&d"), "a%20b%2Fc%26d");
        assert_eq!(encode_component("plain-1.2_~"), "plain-1.2_~");
    }

    #[test]
    fn forbidden_origin_names_the_host() {
        let error = transport_error(HostError::ForbiddenOrigin, "feeds.example.test");
        assert_eq!(
            error.code,
            scryer_plugin_sdk::PluginErrorCode::InvalidConfig
        );
        assert!(error.public_message.contains("feeds.example.test"));
        assert_eq!(
            transport_error(HostError::Timeout, "x").code,
            scryer_plugin_sdk::PluginErrorCode::UpstreamUnavailable
        );
    }
}
