//! Conformance against the real Scryer download-client host, run on the
//! RELEASE artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way the host does.
//! What stays here is what is genuinely this plugin's: its descriptor identity
//! and its artifact name.

use scryer_plugin_conformance::download_client::DownloadClientConformance;

#[test]
fn aria2_release_wasm_conforms_to_the_download_client_host_contract() {
    DownloadClientConformance::new(env!("CARGO_MANIFEST_DIR"), "aria2")
        .wasm("aria2_download_client.wasm")
        .run();
}
