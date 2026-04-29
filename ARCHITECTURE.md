# Scryer Plugins Architecture Manifesto

This repository is the source of truth for distributable Scryer plugin artifacts and their registry metadata.

For humans and agents alike:

- `cargo xtask` is the canonical interface for repo automation
- `cargo xtask release <plugin>` is the release path
- `cargo xtask registry validate` is the registry integrity check
- `cargo xtask plugin validate <path>` is the SDK-v1 ABI check for a plugin crate
- `cargo xtask plugin new <kind> <name>` is the scaffold path for new plugin crates
- `scripts/release.sh` is a compatibility wrapper over xtask, not the source of truth

Operational rules:

- `registry.json` is authoritative for published plugin metadata
- builtin plugins do not get independently released from this repo
- wasm artifacts in `dist/` must match the URLs and SHA-256 hashes recorded in the registry
- new automation belongs in xtask rather than ad hoc shell or Python helpers
