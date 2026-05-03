# Scryer Plugins Architecture Manifesto

This repository is the source of truth for distributable Scryer plugin artifacts and their registry metadata.

For humans and agents alike:

- `cargo xtask` is the canonical interface for repo automation
- `cargo xtask release-changed` is the canonical official release path
- `cargo xtask release <plugin-id>` remains available for one-off legacy release prep
- `cargo xtask registry validate` is the registry integrity check
- `cargo xtask plugin validate <path>` is the SDK-v1 ABI check for a plugin crate
- `cargo xtask plugin new <kind> <name>` is the scaffold path for new plugin crates
- `scripts/release.sh` is a compatibility wrapper over xtask, not the source of truth

Operational rules:

- `registry.json` is legacy-only for pre-0.13.2/local flows and must not be
  mutated by the catalog-v2 release path
- `catalog-v2.json`, one-plugin child catalogs, and per-release manifests are
  the 0.13.2+ runtime distribution contract
- official plugin child catalog assets ship in the same GitHub Release as the
  matching `plugins/<plugin-id>/v*` artifact bundle; do not create a second
  `plugins/<plugin-id>/catalog` release for first-party plugins
- plugin crates and xtask move to the published `scryer-plugin-sdk` crate
  after the SDK release has landed on crates.io; do not add new sibling
  `../scryer` path dependencies after that cutover
- SDK dependency bumps are explicit maintainer actions via
  `cargo xtask sdk bump <version>` after the SDK crate has been published
- release tags are split by product: Scryer app tags use `scryer-v*`, the SDK
  uses `plugin-sdk-v*`, plugin version tags use `plugins/<plugin-id>/v*`, and
  the watched orchestration tag family is `plugins/release/*`
- GitHub Actions must only watch `plugins/release/*`; per-plugin version tags
  are inventory for the batch publisher, not direct workflow triggers
- plugin releases append immutable `releases[]` entries instead of overwriting one flat row
- Scryer owns built-in pinning; this repo can publish official plugins, but it
  no longer declares built-in candidates
- release artifacts are optimized with `wasm-opt -Oz`, compressed with
  `zstd -10`, hashed with BLAKE3, and signed with cosign keyless bundles
- new automation belongs in xtask rather than ad hoc shell or Python helpers
