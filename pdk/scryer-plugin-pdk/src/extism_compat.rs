//! Small source-compatibility layer for first-party Extism plugin migrations.
//!
//! This is not an Extism runtime. It preserves the narrow `Error`, config, and
//! HTTP call shapes the first-party clients already use while routing each
//! operation through the native `scryer:host/v1` service.

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
    /// This intentionally preserves Extism's `Result<Option<String>>` source
    /// shape so callers can either propagate host failures or treat them as a
    /// missing optional setting.
    pub fn get(key: impl Into<String>) -> FnResult<Option<String>> {
        #[cfg(all(target_arch = "wasm32", target_os = "wasi", target_env = "p2"))]
        {
            return Ok(crate::component::config_get(key));
        }

        #[cfg(not(all(target_arch = "wasm32", target_os = "wasi", target_env = "p2")))]
        Ok(super::host::config_get(key)?)
    }
}

/// Typed persistent state compatible with Extism's small `var` helper.
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
    fn request_builder_preserves_extism_source_shape() {
        let request = HttpRequest::new("https://downloader.example/api")
            .with_method("POST")
            .with_header("X-Test", "one");
        assert_eq!(request.method.as_deref(), Some("POST"));
        assert_eq!(request.headers.get("X-Test"), Some(&"one".to_string()));
    }

    #[test]
    fn config_get_preserves_extism_result_shape() {
        let _result: FnResult<Option<String>> = config::get("base_url");
    }

    #[test]
    fn var_preserves_extism_result_shapes() {
        let _get: FnResult<Option<String>> = var::get("key");
        let _set: FnResult<()> = var::set("key", "value");
        let _remove: FnResult<()> = var::remove("key");
    }
}
