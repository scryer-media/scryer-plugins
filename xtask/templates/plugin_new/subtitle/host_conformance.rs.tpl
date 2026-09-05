//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The contract is not "these functions behave" but "this exact `.wasm` runs
//! under Scryer's subtitle host", so the suite builds the shipping
//! `wasm32-wasip2` component and drives it the way
//! `crates/scryer-plugins/src/wasmtime_host/subtitle_component_host.rs` does:
//! the world is linked as `scryer:subtitle/subtitle-provider@1.1.0`, both the
//! encoded `scryer:host/services@1.0.0` door and the typed
//! `scryer:runtime/host@1.0.0` import are served from one scripted host, and
//! WASI Preview 2 comes from the linker. The suite itself lives in
//! `scryer-plugin-conformance`; what belongs here is only what is genuinely
//! this provider's.
//!
//! CI runs this on every pull request touching this directory
//! (`.github/workflows/subtitle-component-conformance.yml`), so add the plugin
//! to that workflow's `paths` and `include` matrix when it is ready.
//!
//! # Growing this file
//!
//! The scaffold starts with the checks a stub can pass. `Download` is switched
//! off because the scaffolded arm refuses; delete that `.without(...)` line
//! once it fetches, and describe the hop:
//!
//! ```ignore
//! .config("api_key", "test-api-key")
//! .validate_route("", 200, br#"{"ok":true}"#.to_vec())
//! .validate_reads_config("api_key")
//! .validate_url_prefix("https://{{plugin_id}}.invalid/api")
//! .download_route("", 200, b"1\n00:00:01,000 --> 00:00:02,000\nHello\n".to_vec())
//! .download_reference(r#"{"url":"https://{{plugin_id}}.invalid/sub.srt"}"#)
//! .download_expects_format("srt")
//! ```
//!
//! A provider whose upstream is several hops scripts an ordered route table
//! instead of one catch-all; see `subtitles/opensubtitles` for that shape.

use scryer_plugin_conformance::subtitle::{Check, SubtitleConformance};

#[test]
fn {{plugin_fn}}_release_wasm_conforms_to_the_subtitle_host_contract() {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "{{plugin_id}}")
        // TODO: delete this once `Download` fetches rather than refusing.
        .without(Check::Download)
        .run();
}
