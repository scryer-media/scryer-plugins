# scryer-plugin-pdk

Guest runtime bindings for **Scryer** WebAssembly plugins.

This crate is the guest half of Scryer's plugin invocation protocols. Every
plugin family now runs as a `wasm32-wasip2` **component**: indexers use typed
host capabilities for single-attempt HTTP, time, configuration, state, and
logging, and the other families exchange the same JSON command envelope over
one `scryer:host/services@1.0.0` door.

There is no Preview 1 path left: the stdin/stdout command runners and their
`main` macros are gone, nothing builds for `wasm32-wasip1`, and no shipped
Scryer host can instantiate a core module.

It serves **Scryer's plugin contract** — it is deliberately *not* a
general-purpose plugin framework. The API promise is "what Scryer's host
provides". Wire types are *not* owned here: the protocol/descriptor/schema types
remain the single source of truth in [`scryer-plugin-sdk`], which this crate
depends on and re-exports.

## What it provides

- Family component entry macros (subtitles, download clients, notifications).
  Each supplies the component's `describe` and `process` exports, installs the
  `scryer:host/services@1.0.0` transport and the stderr log sink, and dispatches
  the JSON command envelope to the plugin's handler.
- Async component bindings and an indexer component macro. Indexers own their
  upstream pacing, quotas, retries, pagination, and fanout.
- A panic hook that reports to stderr (guests build `panic = "abort"`, so the
  process then aborts / the host observes a trap).
- Re-exports of command wire-protocol types from `scryer-plugin-sdk`, plus the
  complete SDK under `scryer_plugin_pdk::sdk` for descriptor construction.

Protocol-level faults (a malformed request, an unserializable response) surface
as the world's `invocation-error`. Operational outcomes are reported **in-band**
through the protocol response, never as an invocation failure.

For download-client command ABI v1, `ListQueue` is the live state snapshot and
may include `failed` or `error` items so the host can trigger failure handling.
`ListHistory` remains the completed-download shape used for imports. The
first-party bridge merges failed/error entries from the legacy history function
into `ListQueue`, with terminal history data taking precedence when the same
client item is present in both sources.

## Usage

A plugin generates its family world from the WIT vendored in its own crate and
hands the entry macro a descriptor factory and a command handler:

```ignore
wit_bindgen::generate!({ world: "notification", path: "wit" });

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = build_descriptor,
    handler = handle_notification_command,
);
```

## Building guest artifacts

### Components

Every family builds a `cdylib` for `wasm32-wasip2` with `panic = "abort"`. The
family entry macro supplies the `describe` and `process` exports; there is no
`main` and no host-crypto import namespace to route.

Scryer's host supports baseline Wasm plus SIMD and relaxed SIMD. The catalog
`feature_sets` metadata in each plugin's `[package.metadata.scryer]` selects a
matching flavor per host. Build each flavor with the target/`RUSTFLAGS` below;
the slugs mirror `required_features`:

| Flavor | `required_features` | Build |
|---|---|---|
| baseline | `[]` | `cargo build --profile plugin-release --target wasm32-wasip2` |
| simd | `["simd128"]` | baseline + `RUSTFLAGS="-C target-feature=+simd128"` |
| relaxed-simd | `["simd128","relaxed-simd"]` | baseline + `RUSTFLAGS="-C target-feature=+simd128,+relaxed-simd"` |

Threads and exception handling are not supported by Scryer's plugin host. Do
not publish plugin flavors that require either feature.

### Indexer components

An indexer is a `cdylib` using `scryer_indexer_component_main!`. Its descriptor
is exported directly by the component. Package components with
`wasm-tools strip` followed by `wasm-tools validate`. The packaging tool then
embeds the descriptor and performs its normal compression and signing steps.

## Versioning

`0.x` during the RFC 123 program. Version 0.4 adds the standardized
descriptor-aware entry points. Releases remain semver-honest, but the API
promise stays "what Scryer's host provides", nothing broader. Publication to
crates.io is owner-triggered.

[`scryer-plugin-sdk`]: https://crates.io/crates/scryer-plugin-sdk
