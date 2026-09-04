# Tribler

Drives Tribler's downloads REST API (`/api/downloads`, `/api/settings`,
`/api/versioning`). Reconciled against Sonarr's `TriblerDownloadClient` and
against Tribler's own endpoint source; where the two disagree, Tribler wins.

Tribler support is experimental — Tribler says so itself, and Sonarr ships a
provider warning to that effect. This plugin was verified against Tribler
**8.0.7** through **8.4.3**, and keeps reading the field names Tribler 7.x used
where they were later renamed.

## Connection and configuration

| Field | Notes |
|---|---|
| `host`, `port` | Tribler's REST API; the 8.x default port is 20100. |
| `use_ssl` | Selects `https`. |
| `url_base` | A URL *path* Tribler is served under, not a URL. |
| `api_key` | The `[api] key` from `triblerd.conf`, sent as `X-Api-Key`. Required. |
| `category` | A child directory under Tribler's save location. Letters and `-` only, with an optional leading dot. Mutually exclusive with `directory` (the connection test reports the combination; if both are set anyway, `directory` wins). |
| `directory` | An explicit destination; a routed per-download directory still wins. |
| `anonymity_level` | Default number of anonymity hops. `0` disables anonymous downloading. |
| `safe_seeding` | Seed only through proxies. Tribler refuses an anonymous download with safe seeding off, so the plugin refuses that combination up front. |

Settings are validated in **Test**: a missing API key, a `url_base` that is
really a URL, an out-of-range category, category *and* directory together, a
non-numeric or negative anonymity level, and the anonymous-without-safe-seeding
combination each fail as `InvalidConfig` naming the field. Tribler's own
failures are typed too: a rejected API key is `AuthFailed`, a redirect or a
missing API path is `InvalidConfig`, a connect or TLS failure is
`UpstreamUnavailable`, `5xx` and timeouts are `Temporary`.

## Adding downloads

Magnet links are preferred, and an `http(s)` torrent URL is passed through as
the `uri` — Tribler resolves both itself. A torrent **file** is also supported,
via `PUT /downloads` with the raw metainfo as the body and the options in the
query string; Sonarr refuses torrent files for Tribler.

`safe_seeding` and `anonymity_level` are per-add overridable by Scryer
(`torrent.safe_seeding`, `torrent.anonymity_hops`); otherwise the configured
values are sent.

## What is reported

- Tribler's `DownloadStatus` names, mapped to Scryer states: hash checking is
  `Verifying`, `SEEDING` is `Seeding`, `MOVING` (a relocation to a completed
  directory) is `Extracting`, `QUEUED`/`METADATA`/`ALLOCATING_DISKSPACE` are
  `Queued`, a stopped-but-incomplete download is `Paused`. An unrecognised
  status keeps polling as `Downloading` and reports itself in the item message.
  The numeric `status_code` is ignored: its values were renumbered between
  Tribler releases.
- `completed_at` and seeding time from Tribler's `time_finished` (8.3.0+).
- `can_remove` as an honest tri-state built from Tribler's global
  `seeding_mode`: `true` only once Tribler has stopped a download at a goal it
  can prove was met, `false` for a goal that provably is not met, and unknown
  when Tribler cannot say — for example a `time` goal on a Tribler that does
  not report `time_finished`. `can_move_files` is data completeness only.
- The configured category, on downloads that actually live under its folder.
- Output roots: the directory adds land in, plus Tribler's own save location.

Downloads are listed once per poll: history is empty because Scryer merges it,
and each download's file list is fetched once per plugin instance rather than
on every poll.

## Limits

Pause and resume are supported (`PATCH /downloads/{infohash}`). Force start,
queue priority, per-download seed limits and start-paused are not: Tribler
either has no such control, or gained it too recently to advertise honestly
across the supported version range.

Tribler has no label, tag, category or view, so an import acknowledgement does
not mutate Tribler at all. Retention and seeding stay with the Tribler client;
removing a finished torrent is Scryer's decision through its seeding gate.
