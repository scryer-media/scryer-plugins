//! A recorded HTTP double and a minimal executor for native unit tests.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use scryer_plugin_sdk::PluginError;
use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};

use crate::error::unavailable;
use crate::http::ListHttp;

/// Answers each request from a table of recorded responses keyed by an
/// exact URL, and records every URL it was asked for. A URL with no recorded
/// response fails as a transport error.
#[derive(Default)]
pub struct RecordedHttp {
    responses: BTreeMap<String, PluginHttpResponse>,
    requests: RefCell<Vec<PluginHttpRequest>>,
}

impl RecordedHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, url: &str, status: u16, body: &str) -> Self {
        self.responses
            .insert(url.to_string(), response(status, &[], body));
        self
    }

    pub fn with_headers(
        mut self,
        url: &str,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Self {
        self.responses
            .insert(url.to_string(), response(status, headers, body));
        self
    }

    pub fn requests(&self) -> Vec<PluginHttpRequest> {
        self.requests.borrow().clone()
    }

    pub fn urls(&self) -> Vec<String> {
        self.requests
            .borrow()
            .iter()
            .map(|request| request.url.clone())
            .collect()
    }
}

pub fn response(status: u16, headers: &[(&str, &str)], body: &str) -> PluginHttpResponse {
    PluginHttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        body: body.as_bytes().to_vec(),
    }
}

impl ListHttp for RecordedHttp {
    async fn send(&self, request: PluginHttpRequest) -> Result<PluginHttpResponse, PluginError> {
        let url = request.url.clone();
        self.requests.borrow_mut().push(request);
        self.responses
            .get(&url)
            .cloned()
            .ok_or_else(|| unavailable(format!("no recorded response for {url}")))
    }
}

/// Drive a future that never waits on real I/O to completion.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..10_000 {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
    panic!("the future waited on something the recorded double never provides");
}
