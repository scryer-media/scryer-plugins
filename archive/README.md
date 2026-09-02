# archive

Plugins kept for reference but no longer built, published, or listed in the
plugin catalog. Nothing under this directory is a plugin family: `xtask`
discovers plugins only under the family directories (`download_clients/`,
`indexers/`, `notifications/`, …), the conformance workflows enumerate
manifests explicitly, and CodeQL globs by family, so an archived crate is
inert until it is moved back.

| Plugin | Archived | Why |
|---|---|---|
| `notifications/trakt` | 2026-09-02 | Superseded by a Trakt OAuth integration built directly into Scryer core. The last published plugin release (0.1.4) is capped at Scryer 0.19.8 in `catalog-v3-release-constraints.json`. |
