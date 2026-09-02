//! The guest-facing convenience API over Scryer's host services.
//!
//! `HttpRequest`, `HttpResponse`, `Error`, `FnResult`, and the [`config`],
//! [`var`] and [`http`] modules are what a plugin body actually calls; each one
//! routes through [`crate::host`], which is the only door to the host that
//! exists. Every name here is re-exported from the crate root, so plugins say
//! `scryer_plugin_pdk::{config, http, var, HttpRequest}` and never name this
//! module.
//!
//! The shapes are deliberately narrow and deliberately stable. They were
//! settled when the first-party plugins were ported and they have not moved
//! since: `config::get` returns `Result<Option<String>>` so a caller can tell a
//! host failure from an unset optional setting, `var` is a typed JSON helper
//! over plugin state, and `http::request` takes a built request plus an
//! optional body. Changing any of these signatures is a breaking change for
//! every plugin in the fleet.

use std::collections::BTreeMap;

pub use anyhow::Error;

use crate::PluginHttpRequest;
use crate::host;

pub type FnResult<T> = Result<T, Error>;

#[derive(Debug, Clone)]
pub struct HttpRequest {
    url: String,
    method: Option<String>,
    pub headers: BTreeMap<String, String>,
}

impl HttpRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: None,
            headers: BTreeMap::new(),
        }
    }

    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    pub fn status_code(&self) -> u16 {
        self.status
    }

    pub fn body(&self) -> Vec<u8> {
        self.body.clone()
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> FnResult<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }

    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    pub fn header(&self, name: impl AsRef<str>) -> Option<&str> {
        self.headers.get(name.as_ref()).map(String::as_str)
    }
}

pub mod config {
    use super::FnResult;

    /// Return one descriptor-bound configuration value.
    ///
    /// The `Result<Option<String>>` shape is deliberate: a caller can either
    /// propagate a host failure or treat the value as a missing optional
    /// setting.
    pub fn get(key: impl Into<String>) -> FnResult<Option<String>> {
        // Two component contracts reach configuration by different imports, so
        // the discriminator is which one this instance actually has, not what
        // it was compiled for: both are `wasm32-wasip2`. A family component's
        // entry macro installs the `scryer:host/services` transport, an
        // indexer component's publishes its own world's `config-get`, and both
        // happen before any plugin code runs. Neither branch may *name* the
        // other world's import, or every component would carry an import its
        // host does not serve; see `component::install_config_get`.
        if super::host::host_call_installed() {
            return Ok(super::host::config_get(key)?);
        }
        if let Some(config_get) = crate::runtime::installed_config_get() {
            return Ok(config_get(&key.into()));
        }
        Ok(super::host::config_get(key)?)
    }
}

/// Typed persistent state, as a small JSON-encoded key/value helper.
pub mod var {
    use super::{FnResult, host};

    pub fn get<T>(key: impl Into<String>) -> FnResult<Option<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        host::state_get(key)?
            .map(|value| serde_json::from_slice(&value).map_err(anyhow::Error::from))
            .transpose()
    }

    pub fn set<T>(key: impl Into<String>, value: T) -> FnResult<()>
    where
        T: serde::Serialize,
    {
        let value = serde_json::to_vec(&value)?;
        let _ = host::state_set(key, value)?;
        Ok(())
    }

    pub fn remove(key: impl Into<String>) -> FnResult<()> {
        let _ = host::state_delete(key)?;
        Ok(())
    }
}

pub mod http {
    use super::{FnResult, HttpRequest, HttpResponse, PluginHttpRequest, host};

    /// Execute an HTTP request through Scryer's descriptor-scoped egress host.
    pub fn request<T: Into<Vec<u8>>>(
        request: &HttpRequest,
        body: Option<T>,
    ) -> FnResult<HttpResponse> {
        let response = host::http(PluginHttpRequest {
            url: request.url.clone(),
            method: request.method.clone(),
            headers: request.headers.clone(),
            body: body.map(Into::into).unwrap_or_default(),
        })?;
        Ok(HttpResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builder_preserves_source_shape() {
        let request = HttpRequest::new("https://downloader.example/api")
            .with_method("POST")
            .with_header("X-Test", "one");
        assert_eq!(request.method.as_deref(), Some("POST"));
        assert_eq!(request.headers.get("X-Test"), Some(&"one".to_string()));
    }

    #[test]
    fn config_get_preserves_result_shape() {
        let _result: FnResult<Option<String>> = config::get("base_url");
    }

    #[test]
    fn var_preserves_result_shapes() {
        let _get: FnResult<Option<String>> = var::get("key");
        let _set: FnResult<()> = var::set("key", "value");
        let _remove: FnResult<()> = var::remove("key");
    }
}
