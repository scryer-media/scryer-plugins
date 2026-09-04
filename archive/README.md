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
| `notifications/sendgrid` | 2026-09-02 | Archived on operator decision (SendGrid dropped its free tier in May 2025; the generic email plugin covers SMTP delivery). Last published release 0.1.3 capped at Scryer 0.19.8. |
| `notifications/simplepush` | 2026-09-02 | Archived on operator decision (Simplepush's free tier is now 10 notifications a month). Last published release 0.1.3 capped at Scryer 0.19.8. |
| `transcoders/remux-streaming-copy` | 2026-09-02 | Experimental scaffold. Scryer core has never carried a transcoder plugin host of any kind, and the crate's `run`/`status` exports answer "unsupported runtime". It was the last crate on `extism-pdk` and the only family `xtask` still built for `wasm32-wasip1`, so it was what kept Preview 1 alive in the tooling. Never published — no `catalog-v3-release-constraints.json` entry applies. |
| `download_clients/utorrent` | 2026-09-02 | Archived on operator decision: µTorrent's ad-supported client and its history (bundled cryptominer, 2015) are not something Scryer wants to support. The plugin had just been reconciled with Sonarr (commit 1c62de2) and stays in git history. Last published release 1.1.2 capped at Scryer 0.19.8. |
| `subtitles/whisper` | 2026-09-02 | Archived on operator decision. Experimental on-device speech-to-text subtitle generator (`SubtitleProviderMode::Generator`, the only generator in the family). `official = false`, so plugin-ci never built it and only the subtitle conformance workflow checked its artifact; never published, so no `catalog-v3-release-constraints.json` entry applies. It was also the only subtitle provider besides enhanced-sync that needed the WASI SDK C toolchain to build. |
