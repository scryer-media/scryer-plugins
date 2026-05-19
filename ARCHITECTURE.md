# Scryer Plugins Architecture Manifesto

This repository is the source of truth for distributable Scryer plugin artifacts and their catalog metadata.

For humans and agents alike:

- `cargo xtask` is the canonical interface for repo automation
- `cargo xtask ci strict` is the canonical repo validation pass for format, audit, and strict clippy
- `cargo xtask release-changed` is the canonical official release path
- `cargo xtask release <plugin-id>` remains available for one-off legacy release prep
- `cargo xtask catalog validate-v2` is the authoritative official published-catalog validation pass
- `cargo xtask plugin validate <path>` is the SDK-v1 ABI check for a plugin crate
- `cargo xtask plugin new <kind> <name>` is the scaffold path for new plugin crates
- `scripts/release.sh` is a compatibility wrapper over xtask, not the source of truth
- official plugin inventory is declared in each plugin crate `Cargo.toml` under
  `package.metadata.scryer`, with `package.description` as the canonical
  published description source

Operational rules:

- `catalog-v2.json`, one-plugin child catalogs, and per-release manifests are
  the 0.13.2+ runtime distribution contract
- `catalog-v2` is the source of truth for official plugin inventory; central
  catalog entries point to child catalogs, and child `releases[]` is the full
  installable history for supported Scryer hosts
- child `releases[]` starts at the `scryer-plugin-sdk` `1.5.x` support line for
  the current supported Scryer era; future SDK lines extend that history, but
  pre-`1.5.x` rows do not belong in catalog-v2
- official plugin child catalog assets ship in the same GitHub Release as the
  matching `plugins/<plugin-id>/v*` artifact bundle; do not create a second
  `plugins/<plugin-id>/catalog` release for first-party plugins
- plugin crates and xtask move to the published `scryer-plugin-sdk` crate
  after the SDK release has landed on crates.io; do not add new sibling
  `../scryer` path dependencies after that cutover
- SDK dependency bumps are explicit maintainer actions via
  `cargo xtask sdk bump <version>` after the SDK crate has been published
- `1.5.x` is the canonical launch SDK line and `1.6.x` is the current line;
  older failed starts do not change the published compatibility contract for current official releases
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
