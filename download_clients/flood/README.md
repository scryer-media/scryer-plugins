# Flood

Flood is a web UI and REST API in front of rTorrent, qBittorrent or
Transmission. This plugin drives that API, so what Scryer sees is whatever the
backend behind Flood reports. Its scope is tag-based: the configured tags decide
which torrents Scryer considers when it polls.

Reconciled against Sonarr's Flood client and against Flood's own current sources
(release line 4.16.x).

## Connection and configuration

Configure **host**, **port** (default 3000), optional **url_base**, **use_ssl**,
**username** and **password**. Logging in is `POST /api/auth/authenticate`; the
`jwt` session cookie Flood returns is cached in plugin state and reused. It is a
one-week JWT, so an expired cookie is normal rather than a bad password: the
plugin re-authenticates once on a 401 before reporting an authentication
failure.

**destination** is the fallback download directory. A request-level directory
wins for that download, which is more than Sonarr can express. If the path is
outside the directories Flood is allowed to write to, Flood answers `403 EACCES`
and the plugin reports that as a configuration problem naming the destination —
not as a login failure.

**tags** are the scope for Scryer-managed torrents; **post_import_tags** are
added after a successful import. The two lists must not overlap, and
`test_connection` rejects a configuration where they do: a torrent carrying both
would leave Scryer's scope the moment it is grabbed. **additional_tags** adds
metadata-derived tags (title slug, title tags, year, indexer, language, network)
to new torrents. **start_on_add** controls whether Flood starts the torrent
immediately.

## Behaviour and limits

**Scope.** A torrent is in scope when it carries every configured tag, and
leaves scope once it carries every post-import tag. With no post-import tags
configured, nothing is excluded.

**Identity.** Flood's add routes answer with an array of hashes, but only the
qBittorrent backend fills it in — the rTorrent gateway returns an empty array
and a `202`. The plugin therefore derives the info hash itself, exactly as
Sonarr's core does before it calls a client: the release's own hash, else the
magnet's `btih` (hex or base32), else SHA-1 over the bencoded `info` dictionary
of the torrent file. Whatever Flood does report wins over the derived value.

**Hash casing.** Flood keys its torrent list by the upper-case info hash, and
its action routes pass the hash straight through to the backend, where
rTorrent's lookup is an exact string match. Scryer's own identity is lower-case,
so the plugin translates at the edge: it resolves the client's own casing from
the torrent list before deleting or re-tagging a torrent.

**Import paths.** For a finished torrent the plugin resolves the real content
paths from `GET /api/torrents/{hash}/contents`. A single-file torrent imports
from the file, a multi-file torrent from the directory its contents share, and a
torrent whose contents diverge at the top level from the download directory. A
finished torrent's file list cannot change, so the resolved paths are cached in
plugin state and cost one request per torrent for the lifetime of the plugin
instance rather than one per poll.

**Post-import handoff is never destructive.** After a successful import the
plugin unions **post_import_tags** onto the torrent's existing tags — matching
case-insensitively, keeping Flood's own casing — and nothing else. Carrying
every post-import tag is what takes a torrent out of Scryer's scope, which is
Flood's equivalent of a category swap. Removing a finished torrent is the core's
decision through its seeding gate; this plugin never removes one at import time,
and it reports `removes_completed_downloads: false`. A torrent that is no longer
in Flood is logged and treated as done, not as a failure.

**Seeding.** Flood exposes no per-torrent seeding limit, so the only goal the
plugin can measure is the one Scryer handed it at add time, which it stashes in
plugin state (Sonarr does the same through its cached `SeedConfiguration`). That
stash lives for the plugin instance's lifetime: after a restart the plugin
reports "unknown" rather than guessing, and the core evaluates the goal itself.

**States.** `checking` and `moving` are reported as verifying, a finished
torrent Flood is actively seeding as seeding, a stopped one as paused, an error
state as a warning with Flood's own message attached, and any status Scryer does
not recognise keeps polling as downloading rather than parking the row. A
torrent being re-hashed is never treated as complete, even when Flood also
reports `complete` — that is Flood's own rule.

**Pause and resume** use Flood's own `POST /api/torrents/stop` and `/start`
routes, which Sonarr's client never calls. Force start and queue priority are
not advertised: Flood's start route honours the backend's queue. Flood's API
exposes no version anywhere under `/api`, so the client version is reported as
unknown; differences between Flood 4.x builds are detected from the response
shape instead (`dateFinished`, `isPrivate`, `percentComplete` and the add
response's hash array are all treated as optional).
