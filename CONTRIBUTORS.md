# Contributing to Scryer Plugins

This file covers contributor setup notes that sit alongside [ARCHITECTURE.md](/Users/jeremy/dev/scryer-media/scryer-plugins/ARCHITECTURE.md).

## Prerequisites

- Rust (stable toolchain) + Cargo for `xtask`
- The Wasm/tooling dependencies required by the plugin you are releasing or validating

## macOS Privacy & Security

If `cargo build`, `cargo xtask`, or other Rust commands stall around `build-script-build`, macOS is likely blocking newly compiled local binaries from your terminal app.

Enable your terminal under `System Settings -> Privacy & Security -> Developer Tools`, then fully quit and reopen it.

`spctl developer-mode enable-terminal` only helps `Terminal.app`. If you use Ghostty, iTerm, WezTerm, or another terminal, you must allow that specific app in the Developer Tools list.

## Repo Automation

Use `cargo xtask` as the canonical interface for repo automation. See [ARCHITECTURE.md](/Users/jeremy/dev/scryer-media/scryer-plugins/ARCHITECTURE.md) for release and registry workflow rules.

## Plugin SDK v1

First-party and third-party plugins must use the SDK-v1 ABI from
`scryer-plugin-sdk`. Do not copy protocol structs into plugin crates.

Required exports are validated from the plugin descriptor:

- `scryer_describe`
- `scryer_indexer_search` for indexers
- `scryer_download_add`, `scryer_download_list_queue`, `scryer_download_list_history`, `scryer_download_list_completed`, `scryer_download_control`, `scryer_download_mark_imported`, `scryer_download_status`, and `scryer_download_test_connection` for download clients
- `scryer_notification_send` for notification plugins
- `scryer_validate_config` plus `scryer_subtitle_search` and `scryer_subtitle_download` for catalog subtitle providers
- `scryer_validate_config` plus `scryer_subtitle_generate` for subtitle generators

Use `cargo xtask plugin new <kind> <name>` to scaffold a plugin and
`cargo xtask plugin validate <path>` before opening a registry change. The
validator builds the Wasm module, calls `scryer_describe`, checks descriptor and
registry identity, rejects wildcard network permissions, and verifies required
exports.

The JSON Schema bundle for non-Rust authors is committed in the Scryer repo at
`crates/scryer-plugin-sdk/schemas/plugin-sdk-v1.schema.json`.
