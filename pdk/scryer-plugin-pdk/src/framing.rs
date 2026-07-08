//! Wire framing for the archive command protocol (RFC 123 §7.2.5).
//!
//! The transport is deliberately isolated in this one module. The shipped
//! transport is WASI-command style — one [`ArchivePluginProcessRequest`] JSON
//! document on stdin, exactly one [`ArchivePluginProcessResponse`] JSON
//! document on stdout. If the host spike shows stdin/stdout capture misbehaves
//! under `wasmtime-wasi`, the documented fallback (request/response files in a
//! dedicated rw control preopen, same JSON shapes) is a contained change here:
//! only the `Read`/`Write` handed to [`process`] changes, never its callers or
//! the plugin handler.

use std::fmt;
use std::io::{self, Read, Write};

use scryer_plugin_sdk::{ArchivePluginProcessRequest, ArchivePluginProcessResponse};

/// A protocol-level failure. These are distinct from operational failures,
/// which the handler reports in-band via [`ArchivePluginProcessResponse::status`].
/// Every variant maps to a non-zero process exit; the host attaches the stderr
/// tail and surfaces a plugin-protocol error (RFC 123 §7.2.8).
#[derive(Debug)]
pub enum FramingError {
    /// Reading the request document from the input transport failed.
    ReadRequest(io::Error),
    /// The request document was not a valid `ArchivePluginProcessRequest`.
    ParseRequest(serde_json::Error),
    /// The response could not be serialized to JSON.
    SerializeResponse(serde_json::Error),
    /// Writing the response document to the output transport failed.
    WriteResponse(io::Error),
}

impl FramingError {
    /// Non-zero process exit code for this failure. The host keys on
    /// `exit != 0` + the stderr tail, not on the specific value.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl fmt::Display for FramingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FramingError::ReadRequest(error) => {
                write!(f, "failed to read request from stdin: {error}")
            }
            FramingError::ParseRequest(error) => {
                write!(f, "failed to parse ArchivePluginProcessRequest: {error}")
            }
            FramingError::SerializeResponse(error) => {
                write!(
                    f,
                    "failed to serialize ArchivePluginProcessResponse: {error}"
                )
            }
            FramingError::WriteResponse(error) => {
                write!(f, "failed to write response to stdout: {error}")
            }
        }
    }
}

impl std::error::Error for FramingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FramingError::ReadRequest(error) | FramingError::WriteResponse(error) => Some(error),
            FramingError::ParseRequest(error) | FramingError::SerializeResponse(error) => {
                Some(error)
            }
        }
    }
}

/// Transport-agnostic core of the command protocol.
///
/// Reads the request document from `input` to EOF, dispatches it to `handler`,
/// serializes the returned response, writes it to `output`, and flushes.
/// Callers own process exit and stderr reporting.
pub fn process<R, W, H>(mut input: R, mut output: W, handler: H) -> Result<(), FramingError>
where
    R: Read,
    W: Write,
    H: FnOnce(ArchivePluginProcessRequest) -> ArchivePluginProcessResponse,
{
    let mut buffer = Vec::new();
    input
        .read_to_end(&mut buffer)
        .map_err(FramingError::ReadRequest)?;

    let request: ArchivePluginProcessRequest =
        serde_json::from_slice(&buffer).map_err(FramingError::ParseRequest)?;

    let response = handler(request);

    let encoded = serde_json::to_vec(&response).map_err(FramingError::SerializeResponse)?;
    output
        .write_all(&encoded)
        .map_err(FramingError::WriteResponse)?;
    // Explicit flush: `proc_exit`/WASI abort does not flush libc/std buffers.
    output.flush().map_err(FramingError::WriteResponse)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        ArchivePluginFormat, ArchivePluginOperation, ArchivePluginProcessResponse,
        ArchivePluginStatus,
    };

    fn ok_response() -> ArchivePluginProcessResponse {
        ArchivePluginProcessResponse {
            status: ArchivePluginStatus::Ok,
            files: vec![],
            repair: None,
            expanded_bytes: Some(42),
            copied_bytes: None,
            staged_bytes: None,
            error_code: None,
            message: None,
        }
    }

    #[test]
    fn round_trips_a_verify_request_through_the_handler() {
        let request = ArchivePluginProcessRequest {
            operation: ArchivePluginOperation::VerifyRepairSet {
                source_dir: "/scryer/source".to_string(),
                par2_path: Some("/scryer/source/set.par2".to_string()),
            },
        };
        let input = serde_json::to_vec(&request).unwrap();
        let mut output = Vec::new();

        let mut seen_source = String::new();
        process(input.as_slice(), &mut output, |request| {
            if let ArchivePluginOperation::VerifyRepairSet { source_dir, .. } = request.operation {
                seen_source = source_dir;
            }
            ok_response()
        })
        .expect("process should succeed");

        assert_eq!(seen_source, "/scryer/source");
        let decoded: ArchivePluginProcessResponse = serde_json::from_slice(&output).unwrap();
        assert_eq!(decoded.status, ArchivePluginStatus::Ok);
        assert_eq!(decoded.expanded_bytes, Some(42));
    }

    #[test]
    fn emits_exactly_one_response_document() {
        let request = ArchivePluginProcessRequest {
            operation: ArchivePluginOperation::ExtractArchive {
                archive_path: "/scryer/source/a.zip".to_string(),
                output_dir: "/scryer/output".to_string(),
                format: ArchivePluginFormat::Zip,
                password: None,
            },
        };
        let input = serde_json::to_vec(&request).unwrap();
        let mut output = Vec::new();
        process(input.as_slice(), &mut output, |_| ok_response()).unwrap();

        // Response is a single JSON value with no trailing bytes.
        let mut de = serde_json::Deserializer::from_slice(&output).into_iter::<serde_json::Value>();
        assert!(de.next().is_some(), "one response document expected");
        assert!(de.next().is_none(), "no trailing document expected");
    }

    #[test]
    fn malformed_request_is_a_parse_error() {
        let mut output = Vec::new();
        let error = process(&b"not json"[..], &mut output, |_| ok_response())
            .expect_err("malformed request should fail");
        assert!(matches!(error, FramingError::ParseRequest(_)));
        assert_ne!(error.exit_code(), 0);
        assert!(
            output.is_empty(),
            "no response should be written on parse failure"
        );
    }

    #[test]
    fn read_failure_is_surfaced() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "boom"))
            }
        }
        let mut output = Vec::new();
        let error = process(FailingReader, &mut output, |_| ok_response())
            .expect_err("read failure should fail");
        assert!(matches!(error, FramingError::ReadRequest(_)));
        assert_ne!(error.exit_code(), 0);
    }

    #[test]
    fn write_failure_is_surfaced() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "boom"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let request = ArchivePluginProcessRequest {
            operation: ArchivePluginOperation::VerifyRepairSet {
                source_dir: "/scryer/source".to_string(),
                par2_path: None,
            },
        };
        let input = serde_json::to_vec(&request).unwrap();
        let error = process(input.as_slice(), FailingWriter, |_| ok_response())
            .expect_err("write failure should fail");
        assert!(matches!(error, FramingError::WriteResponse(_)));
    }
}
