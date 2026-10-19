//! [`HttpScript::Live`](crate::HttpScript::Live): the plugin's outbound request,
//! performed for real.
//!
//! A scripted route proves the plugin sends what the contract says; it cannot
//! prove the upstream agrees. A live check closes that gap against a service
//! the developer runs locally, so this is deliberately plain HTTP with no TLS,
//! no redirects and no cookie jar: the plugin sees exactly what the upstream
//! said, the way it would through Scryer's own host.

use std::collections::BTreeMap;
use std::time::Duration;

use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{PluginError, PluginErrorCode, PluginResult};
use ureq::http::{Request, request::Builder};

/// Upper bound on one request, so a hung upstream fails the check instead of
/// hanging it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound on one response body.
const BODY_LIMIT: u64 = 64 * 1024 * 1024;

/// Headers the client computes from the body itself; forwarding the plugin's
/// copy would duplicate them on the wire.
const CLIENT_OWNED_HEADERS: [&str; 2] = ["content-length", "transfer-encoding"];

pub(crate) fn perform(
    method: &str,
    request: &PluginHttpRequest,
) -> PluginResult<PluginHttpResponse> {
    match send(method, request) {
        Ok(response) => PluginResult::Ok(response),
        Err(message) => PluginResult::Err(upstream_unavailable(&format!(
            "live HTTP {method} {} failed: {message}",
            request.url
        ))),
    }
}

fn send(method: &str, request: &PluginHttpRequest) -> Result<PluginHttpResponse, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        // The plugin owns status handling; a 4xx/5xx is an answer, not a
        // transport failure.
        .http_status_as_error(false)
        .max_redirects(0)
        .allow_non_standard_methods(true)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into();

    let mut builder: Builder = Request::builder().method(method).uri(&request.url);
    for (name, value) in &request.headers {
        if CLIENT_OWNED_HEADERS
            .iter()
            .any(|owned| name.eq_ignore_ascii_case(owned))
        {
            continue;
        }
        builder = builder.header(name, value);
    }

    let result = if request.body.is_empty() {
        let built = builder.body(()).map_err(|error| error.to_string())?;
        agent.run(built)
    } else {
        let built = builder
            .body(request.body.clone())
            .map_err(|error| error.to_string())?;
        agent.run(built)
    };
    let mut response = result.map_err(|error| error.to_string())?;

    let status = response.status().as_u16();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in response.headers() {
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        headers
            .entry(name.as_str().to_string())
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(&value);
            })
            .or_insert(value);
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
        .map_err(|error| error.to_string())?;

    Ok(PluginHttpResponse {
        status,
        headers,
        body,
    })
}

/// Every optional field is filled for the same postcard reason documented on
/// [`crate::unsupported`].
fn upstream_unavailable(message: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::UpstreamUnavailable,
        public_message: message.to_string(),
        debug_message: Some(message.to_string()),
        retry_after_seconds: Some(0),
        details: None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use scryer_plugin_sdk::host::{PluginHostRequest, PluginHostResponse};

    use crate::{HttpScript, Script, default_respond};

    use super::*;

    /// What the loopback upstream saw.
    struct Seen {
        request_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// A one-shot HTTP/1.1 upstream on loopback that records the request and
    /// answers with `reply` verbatim.
    fn one_shot_upstream(reply: &'static str) -> (String, thread::JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback upstream");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept the plugin's request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("read request line");
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read header line");
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').expect("header has a colon");
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
            let length = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .map(|(_, value)| value.parse::<usize>().expect("numeric content-length"))
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("read request body");
            let mut stream = stream;
            stream.write_all(reply.as_bytes()).expect("write the reply");
            Seen {
                request_line: request_line.trim_end().to_string(),
                headers,
                body,
            }
        });
        (base, handle)
    }

    fn http(url: String, method: Option<&str>, body: &[u8]) -> PluginHostRequest {
        let mut headers = BTreeMap::new();
        headers.insert("X-Plugin".to_string(), "metube".to_string());
        headers.insert("Content-Length".to_string(), "999".to_string());
        PluginHostRequest::Http(PluginHttpRequest {
            url,
            method: method.map(str::to_string),
            headers,
            body: body.to_vec(),
        })
    }

    #[test]
    fn a_live_request_is_performed_and_the_real_reply_returned() {
        let (base, upstream) = one_shot_upstream(
            "HTTP/1.1 418 I'm a teapot\r\nContent-Length: 5\r\nX-Probe: yes\r\nConnection: close\r\n\r\nhello",
        );
        let url = format!("{base}/add?x=1");
        let mut script = Script {
            http: HttpScript::Live,
            ..Script::default()
        };

        let response = default_respond(http(url.clone(), Some("POST"), b"{\"a\":1}"), &mut script)
            .expect("a live request is in-band, not a transport failure");
        let PluginHostResponse::Http(PluginResult::Ok(reply)) = response else {
            panic!("a real upstream reply must come back as Ok: {response:?}");
        };
        assert_eq!(
            reply.status, 418,
            "a non-2xx status is an answer, not an error"
        );
        assert_eq!(reply.body, b"hello");
        assert_eq!(
            reply.headers.get("x-probe").map(String::as_str),
            Some("yes")
        );

        let seen = upstream.join().expect("upstream thread");
        assert_eq!(seen.request_line, "POST /add?x=1 HTTP/1.1");
        assert!(
            seen.headers
                .iter()
                .any(|(name, value)| name == "x-plugin" && value == "metube"),
            "the plugin's headers reach the upstream: {:?}",
            seen.headers
        );
        assert_eq!(
            seen.headers
                .iter()
                .filter(|(name, _)| name == "content-length")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["7"],
            "the client computes Content-Length from the body"
        );
        assert_eq!(seen.body, b"{\"a\":1}");

        // Recorded exactly like the scripted modes.
        assert_eq!(script.urls, vec![url.clone()]);
        assert_eq!(script.calls, vec![format!("http:{url}")]);
        assert_eq!(script.requests[0].method.as_deref(), Some("POST"));
        assert_eq!(script.requests[0].body, b"{\"a\":1}");
    }

    #[test]
    fn a_live_get_without_a_method_defaults_to_get() {
        let (base, upstream) = one_shot_upstream(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        );
        let mut script = Script {
            http: HttpScript::Live,
            log_http_method: true,
            ..Script::default()
        };
        let url = format!("{base}/version");
        let response = default_respond(http(url.clone(), None, b""), &mut script).expect("in-band");
        assert!(matches!(
            response,
            PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse { status: 200, .. }))
        ));
        assert_eq!(
            upstream.join().expect("upstream thread").request_line,
            "GET /version HTTP/1.1"
        );
        assert_eq!(script.calls, vec![format!("http:GET {url}")]);
    }

    #[test]
    fn a_live_transport_failure_is_upstream_unavailable_in_band() {
        // An upstream that accepts and hangs up without a byte of reply: a
        // transport failure that stays on loopback and cannot race another
        // process for the port.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback upstream");
        let addr = listener.local_addr().expect("local addr");
        let upstream = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept the plugin's request");
            drop(stream);
        });
        let url = format!("http://{addr}/history");
        let mut script = Script {
            http: HttpScript::Live,
            ..Script::default()
        };
        let response = default_respond(http(url.clone(), None, b""), &mut script).expect("in-band");
        let PluginHostResponse::Http(PluginResult::Err(error)) = response else {
            panic!("a refused connection must be an in-band error: {response:?}");
        };
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
        assert!(error.public_message.contains(&url));
        assert_eq!(script.urls, vec![url]);
        upstream.join().expect("upstream thread");
    }
}
