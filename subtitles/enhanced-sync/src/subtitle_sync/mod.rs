//! Scryer subtitle-sync engine.
//!
//! The alignment, framerate search, subtitle speech extraction, and metadata
//! filtering behavior are derived from an MIT-licensed upstream subtitle sync
//! implementation. This crate is distributed under its package GPL-3.0-only
//! license.

mod aligners;
pub(crate) mod simd;
mod subtitles;
mod sync;
mod transformers;
mod vad;

#[cfg(test)]
mod upstream_parity_tests;

pub(crate) use sync::{SyncError, SyncOptions, sync_subtitle};
pub(crate) use transformers::Span;
pub(crate) use vad::{SpeechDetection, SpeechSpanDetector, WEBRTC_BACKEND_LABEL};
