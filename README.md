# Scryer Plugins

[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/scryer-media/scryer-plugins/badge)](https://scorecard.dev/viewer/?uri=github.com/scryer-media/scryer-plugins)

This repository contains Scryer’s first-party plugins and the shared tooling used to build, validate, and release them.

Plugins are grouped by capability, with common guest-runtime support in the PDK and release tooling in `xtask`. Official plugin releases are built from this repository and published through the Scryer plugin catalog.

## Community rule packs

Rule packs are JSON manifests under `rule_packs/`, registered in
`rule_packs/manifest.json`. They can be updated independently of WASM plugins.
A pack manifest may set `customizable` to a JSON boolean, which tells hosts
whether they may offer user customization for that pack. The field defaults to
`true` when omitted.
For generated packs, update the saved source data, review the coverage changes,
and regenerate with a new pack version. See the [SeaDex update instructions](rule_packs/seadex/README.md).
Published versions are immutable; changes to pack content or the minimum Scryer
version require a new pack version. The catalog retains previous releases.

After the update PR is merged, publication is a separate operator action. From
the clean merge commit, an explicitly authorized pack publication uses:

```sh
cargo xtask release-publish-tags --pr <number> --rule-pack-id <pack-id>
```

Repeat `--rule-pack-id` to select multiple packs; add `--plugin-id` to include a
plugin in the same publication. The command creates and verifies signed
`rule-packs/<pack-id>/v<version>` component tags and the existing signed
`plugins-v3/release/*` trigger. A pack-only publication skips plugin builds and
preserves existing plugin catalog entries. The workflow signs the compressed
pack assets and catalog, then publishes them through the existing CDN and GitHub
catalog mirrors. Manual workflow dispatch remains diagnostic only.
