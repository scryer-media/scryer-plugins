# uTorrent

This plugin drives the uTorrent Web UI API. It obtains and caches the Web UI
token and cookie, then uses uTorrent labels as Scryer's isolation boundary.

uTorrent has shipped bundled cryptominers, malware and ads; Scryer reports that
as a client warning, and a different torrent client is worth considering.

## Connection and configuration

Set **host**, **port** (default 8080), optional **url_base**, and **use_ssl**,
plus the Web UI **username** and **password**. **category** defaults to
scryer-tv and filters queue and completed polling, matched exactly against the
torrent's uTorrent label. Labels are reported back in uTorrent's own casing.

**recent_priority** and **older_priority** choose whether recent or older
releases are moved to the top of uTorrent's queue. Scryer can also route an
explicit queue placement for one grab, and that wins: first sends `queuetop`,
last sends `queuebottom`.

**initial_state** defaults to start and is sent as the Web UI action after
adding a torrent. A state routed by Scryer for one grab wins over it — paused
sends `pause`, stopped sends `stop`, started sends `start`, and a force-start
request sends `forcestart`.

**post_import_category** is optional; see *Post-import handling* below.

uTorrent builds older than 25406 (uTorrent 3.0) are rejected by the connection
test: their `list=1` rows stop before the status message and root download path,
so Scryer cannot locate a download's output.

## Sources and info hashes

Magnets, torrent URLs, torrent files and raw torrent bytes are all accepted, in
that order of preference. A magnet leads because uTorrent fetches it itself; a
core-supplied `.torrent` body comes next, because a plugin-side GET of a torrent
URL carries none of the indexer's cookies or rate-limit budget. Handing uTorrent
a bare torrent URL is the last resort, and only works for indexers that need no
authentication on the download link.

uTorrent's `add-url` and `add-file` return nothing but a build number, so the
info hash Scryer tracks the download by has to be known before the add. The
plugin uses, in order:

1. the info hash the release already carries;
2. the magnet's `btih` value, hex or base32;
3. SHA-1 over the bencoded `info` dictionary of the torrent body.

A plain torrent URL with no hash available anywhere is refused rather than added
untracked.

## Polling

Each poll sends one `list=1` request. After the first poll the plugin passes
uTorrent's cache id back as `cid`, so uTorrent answers with only the changed and
removed torrents and the plugin merges them into a cached list. The cache is
scoped to host, port and category and expires after 15 minutes; a queue too
large for the plugin state budget, or a host with no state service, falls back
to the full listing. uTorrent keeps no separate failed history — a failed
torrent stays in `list=1` with its error flag set — so the history listing is
empty and costs no request.

## Post-import handling

Post-import handling is label-based and never destructive. When
**post_import_category** is set and differs from the label the download was
grabbed under, the grab label is dropped from uTorrent's label list and the
torrent is relabelled to the post-import category. Nothing is stopped, removed
or deleted; whether a finished torrent may be removed is Scryer's decision
through its seeding gate, and this client reports that it does not remove
completed downloads itself.

Note the ordering: Sonarr sets the imported label first and then runs uTorrent's
label-removal trick on the grab label, which relies on the multi-label support
of uTorrent 3.3+. On a single-label build (3.0 to 3.2, still within this
plugin's version floor) `setprops` applies its `s`/`v` pairs in order and that
sequence ends with an empty label. This plugin issues the same two requests the
other way round, which yields the same end state on both kinds of build: the
grab label leaves the label list and the torrent carries the label the setting
names.

## Behavior and limits

Scryer can pause, resume or force-start a torrent, remove it with or without
data, and apply seed ratio or seed-time limits (`seed_override` plus
`seed_ratio`/`seed_time`). uTorrent has no per-download-directory feature
through this adapter, so its own download layout must be usable for imports; the
plugin reports uTorrent's raw remote paths, keeping each path's own separator
style, and Scryer's remote path mappings translate them.

`list=1` exposes no seeding limits and no private-torrent flag, so the plugin
answers honestly rather than guessing: a finished torrent uTorrent has stopped
can be removed, one uTorrent is still running cannot, and a paused or queued one
is reported as unknown for Scryer to decide. The private flag is never claimed.
Completion times are reported when uTorrent supplies them.
