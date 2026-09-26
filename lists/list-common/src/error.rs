//! Plugin errors shaped for the host's list failure classes.
//!
//! The host reads only the error code, the public message and the retry
//! hint: `AuthFailed` asks the operator to fix the key, `RateLimited` pauses
//! the provider for `retry_after_seconds`, `UpstreamUnavailable` and
//! `Temporary` retry on the next interval, and a `Permanent` error whose
//! message says "not found" marks the list as gone. Every other `Permanent`
//! error is a plain failure.

use scryer_plugin_sdk::host::PluginHttpResponse;
use scryer_plugin_sdk::{PluginError, PluginErrorCode};

use crate::http::header;

/// Retry hint used when a 429 carries no usable `Retry-After`.
pub const DEFAULT_RETRY_AFTER_SECONDS: i64 = 300;

pub fn plugin_error(code: PluginErrorCode, message: impl Into<String>) -> PluginError {
    PluginError {
        code,
        public_message: message.into(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}

/// The list, person, company or feed does not exist (or is not visible).
pub fn not_found(what: impl AsRef<str>) -> PluginError {
    plugin_error(
        PluginErrorCode::Permanent,
        format!("{} not found", what.as_ref()),
    )
}

pub fn rate_limited(retry_after_seconds: Option<i64>) -> PluginError {
    let seconds = retry_after_seconds
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS);
    PluginError {
        retry_after_seconds: Some(seconds),
        ..plugin_error(
            PluginErrorCode::RateLimited,
            format!("rate limited by the provider; retry after {seconds} seconds"),
        )
    }
}

pub fn auth_failed(message: impl Into<String>) -> PluginError {
    plugin_error(PluginErrorCode::AuthFailed, message)
}

pub fn unavailable(message: impl Into<String>) -> PluginError {
    plugin_error(PluginErrorCode::UpstreamUnavailable, message)
}

pub fn invalid_config(message: impl Into<String>) -> PluginError {
    plugin_error(PluginErrorCode::InvalidConfig, message)
}

pub fn permanent(message: impl Into<String>) -> PluginError {
    plugin_error(PluginErrorCode::Permanent, message)
}

pub fn unsupported_source(source_type: &str) -> PluginError {
    plugin_error(
        PluginErrorCode::Unsupported,
        format!("unknown list source type {source_type}"),
    )
}

pub fn missing_param(key: &str) -> PluginError {
    invalid_config(format!("missing required list parameter {key}"))
}

/// How a provider authenticates, which decides what a 401 means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The request carried the server API key, so a 401 is a bad key.
    ServerKey,
    /// The request was anonymous, so a 401 means the source is private.
    Public,
}

/// Parse a `Retry-After` header given in delta seconds. HTTP-date values
/// fall back to [`DEFAULT_RETRY_AFTER_SECONDS`].
pub fn retry_after_seconds(response: &PluginHttpResponse) -> Option<i64> {
    header(response, "retry-after")?.trim().parse::<i64>().ok()
}

/// Map a non-success status onto the error the host expects. `what` names
/// the thing that was requested, for the not-found message.
pub fn check_status(
    response: &PluginHttpResponse,
    access: Access,
    what: &str,
) -> Result<(), PluginError> {
    let status = response.status;
    match status {
        200..=299 => Ok(()),
        401 if access == Access::ServerKey => Err(auth_failed(format!(
            "the provider rejected the API key (HTTP {status})"
        ))),
        401 | 403 => Err(not_found(format!(
            "{what} (private or unavailable, HTTP {status})"
        ))),
        404 | 410 => Err(not_found(format!("{what} (HTTP {status})"))),
        429 => Err(rate_limited(retry_after_seconds(response))),
        408 | 500..=599 => Err(unavailable(format!(
            "the provider is unavailable (HTTP {status})"
        ))),
        _ => Err(permanent(format!(
            "the provider answered with unexpected HTTP {status}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn response(status: u16, headers: &[(&str, &str)]) -> PluginHttpResponse {
        PluginHttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>(),
            body: Vec::new(),
        }
    }

    #[test]
    fn statuses_map_to_host_failure_classes() {
        let code = |status, access| {
            check_status(&response(status, &[]), access, "list 7")
                .unwrap_err()
                .code
        };
        assert_eq!(code(401, Access::ServerKey), PluginErrorCode::AuthFailed);
        assert_eq!(code(401, Access::Public), PluginErrorCode::Permanent);
        assert_eq!(code(403, Access::ServerKey), PluginErrorCode::Permanent);
        assert_eq!(code(404, Access::Public), PluginErrorCode::Permanent);
        assert_eq!(code(429, Access::Public), PluginErrorCode::RateLimited);
        assert_eq!(
            code(503, Access::Public),
            PluginErrorCode::UpstreamUnavailable
        );
        assert_eq!(code(418, Access::Public), PluginErrorCode::Permanent);
        assert!(check_status(&response(204, &[]), Access::Public, "x").is_ok());
    }

    #[test]
    fn not_found_messages_are_recognised_by_the_host() {
        for status in [403, 404, 410] {
            let error = check_status(&response(status, &[]), Access::Public, "list 7").unwrap_err();
            assert!(error.public_message.contains("not found"), "{status}");
        }
        let unexpected = check_status(&response(418, &[]), Access::Public, "x").unwrap_err();
        assert!(!unexpected.public_message.contains("not found"));
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let error = check_status(
            &response(429, &[("Retry-After", "42")]),
            Access::ServerKey,
            "x",
        )
        .unwrap_err();
        assert_eq!(error.retry_after_seconds, Some(42));

        let dated = check_status(
            &response(429, &[("retry-after", "Wed, 21 Oct 2037 07:28:00 GMT")]),
            Access::ServerKey,
            "x",
        )
        .unwrap_err();
        assert_eq!(dated.retry_after_seconds, Some(DEFAULT_RETRY_AFTER_SECONDS));
    }
}
