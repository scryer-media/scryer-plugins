# scryer-plugin-pdk

Guest runtime bindings for **Scryer** WebAssembly plugins.

This crate is the guest half of Scryer's plugin invocation protocols. Legacy
plugin kinds run as `wasm32-wasip1` **commands**: the host writes one request
document to stdin and reads one response document from stdout. Indexers run as
long-lived `wasm32-wasip2` **components** and use typed host capabilities for
single-attempt HTTP, time, configuration, state, and logging.

It serves **Scryer's plugin contract** — it is deliberately *not* a
general-purpose plugin framework. The API promise is "what Scryer's host
provides". Wire types are *not* owned here: the protocol/descriptor/schema types
remain the single source of truth in [`scryer-plugin-sdk`], which this crate
depends on and re-exports.

## What it provides

- Descriptor-aware archive and subtitle-sync runners and `main` macros. With a
  `describe` argument they lazily build and write one `PluginDescriptor`; with
  no argument they dispatch the normal request/response protocol.
- Compatibility runners and macro forms for command plugins that still own
  descriptor dispatch themselves.
- Async component bindings and an indexer component macro. Indexers own their
  upstream pacing, quotas, retries, pagination, and fanout.
- A panic hook that reports to stderr (guests build `panic = "abort"`, so the
  process then aborts / the host observes a trap).
- Re-exports of command wire-protocol types from `scryer-plugin-sdk`, plus the
  complete SDK under `scryer_plugin_pdk::sdk` for descriptor construction.

Protocol-level faults (malformed request, unwritable stdout) go to stderr and
exit non-zero. Operational outcomes are reported **in-band** through the
protocol response, never by exiting non-zero.

For download-client command ABI v1, `ListQueue` is the live state snapshot and
may include `failed` or `error` items so the host can trigger failure handling.
`ListHistory` remains the completed-download shape used for imports. The
first-party compatibility bridge merges failed/error entries from the legacy
history function into `ListQueue`, with terminal history data taking precedence
when the same client item is present in both sources.

The stdin/stdout transport is isolated in one module (`framing`). If a host
spike shows stdin/stdout capture misbehaves under `wasmtime-wasi`, the
documented fallback (request/response files in a dedicated rw control preopen,
same JSON) is a contained change to that module only.

## Usage

```rust
use scryer_plugin_pdk::{ArchivePluginProcessRequest, ArchivePluginProcessResponse};

fn descriptor() -> scryer_plugin_pdk::sdk::PluginDescriptor {
    // ... construct the plugin descriptor ...
    unimplemented!()
}

fn handle(request: ArchivePluginProcessRequest) -> ArchivePluginProcessResponse {
    // ... inspect / extract / verify-repair / repair-then-extract ...
    unimplemented!()
}

scryer_plugin_pdk::scryer_archive_plugin_main!(
    descriptor = descriptor,
    handler = handle,
);
```

## Building guest artifacts

### Legacy command plugins

The plugin is a **command** binary: it needs a `main` (the macro provides one)
and is built for a `wasm32-wasip1` target with `panic = "abort"`. The resulting
module exports `_start` and `memory`. For the archive plugin it imports exactly
the two host crypto functions (`host_aes_cbc_decrypt`, `host_crc32`; RFC 123
§5). `unrar-rs` defaults those imports to the neutral `host` namespace;
Scryer builds it with `host-abi-extism` to route them through
`extism:host/user`. The namespace is compatibility routing only: the guest has
no Extism dependency.

Scryer's host supports baseline Wasm plus SIMD and relaxed SIMD. The catalog
`feature_sets` metadata in each plugin's `[package.metadata.scryer]` selects a
matching flavor per host. Build each flavor with the target/`RUSTFLAGS` below;
the slugs mirror `required_features`:

| Flavor | `required_features` | Build |
|---|---|---|
| baseline | `[]` | `cargo build --profile plugin-release --target wasm32-wasip1` |
| simd | `["simd128"]` | baseline + `RUSTFLAGS="-C target-feature=+simd128"` |
| relaxed-simd | `["simd128","relaxed-simd"]` | baseline + `RUSTFLAGS="-C target-feature=+simd128,+relaxed-simd"` |

Threads and exception handling are not supported by Scryer's plugin host. Do
not publish plugin flavors that require either feature.

Legacy core-module artifacts use `wasm-opt` before compression and descriptor
embedding.

### Indexer components

An indexer is a `cdylib` using `scryer_indexer_component_main!` and is built
with `cargo build --profile plugin-release --target wasm32-wasip2`. Its
descriptor is exported directly by the component. Package components with
`wasm-tools strip` followed by `wasm-tools validate`; Binaryen `wasm-opt` does
not support component binaries and must not be run on them. The packaging tool
then embeds the descriptor and performs its normal compression and signing
steps.

## Versioning

`0.x` during the RFC 123 program. Version 0.4 adds the standardized
descriptor-aware entry points. Releases remain semver-honest, but the API
promise stays "what Scryer's host provides", nothing broader. Publication to
crates.io is owner-triggered.

[`scryer-plugin-sdk`]: https://crates.io/crates/scryer-plugin-sdk
