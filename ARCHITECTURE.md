# Scryer Plugins Architecture Manifesto

This repository is the source of truth for distributable Scryer plugin artifacts and their catalog metadata.

For humans and agents alike:

- `cargo xtask` is the canonical interface for repo automation
- `cargo xtask ci strict` is the canonical repo validation pass for format, audit, and strict clippy
- `cargo xtask release-changed` is the canonical official release path
- `cargo xtask release <plugin-id>` remains available for one-off official release prep
- `cargo xtask catalog render-v3` is the local central catalog validation/render pass
- `cargo xtask plugin validate <path>` is the current SDK ABI check for a plugin crate
- `cargo xtask plugin new <kind> <name>` is the scaffold path for new plugin crates
- release automation must stay in `cargo xtask`; do not add shell wrappers
- official plugin inventory is declared in each plugin crate `Cargo.toml` under
  `package.metadata.scryer`, with `package.description` as the canonical
  published description source

Operational rules:

- Catalog-v3 is the only active plugin distribution contract in this repo
- Catalog-v2 artifacts are frozen historical assets; do not prepare, validate,
  publish, or update them from current automation
- official plugin catalog-v3 snippets ship in the same GitHub Release as the
  matching `plugins-v3/<plugin-id>/v*` artifact bundle
- plugin crates and xtask move to the published `scryer-plugin-sdk` crate
  after the SDK release has landed on crates.io; do not add new sibling
  `../scryer` path dependencies after that cutover
- SDK dependency bumps are explicit maintainer actions via
  `cargo xtask sdk bump <version>` after the SDK crate has been published
- current source tracks the published SDK line used by catalog-v3 releases;
  historical catalog-v2 SDK lines do not change current publishing rules
- release tags are split by product: Scryer app tags use `scryer-v*`, the SDK
  uses `plugin-sdk-v*`, plugin version tags use `plugins-v3/<plugin-id>/v*`, and
  the watched orchestration tag family is `plugins-v3/release/*`
- GitHub Actions must only watch `plugins-v3/release/*`; per-plugin version tags
  are inventory for the batch publisher, not direct workflow triggers
- plugin releases append immutable `releases[]` entries instead of overwriting one flat row
- Scryer owns built-in pinning; this repo can publish official plugins, but it
  no longer declares built-in candidates
- release artifacts are optimized with `wasm-opt -Oz`, compressed with
  `zstd -19`, hashed with BLAKE3, and signed with cosign keyless bundles
- new automation belongs in xtask rather than ad hoc shell or Python helpers
