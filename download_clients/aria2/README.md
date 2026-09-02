# aria2

This plugin drives an aria2 daemon through its XML-RPC endpoint. It is for
torrent acquisition and also accepts direct torrent sources; it is not an NZB
client. aria2 1.34.0 or newer is required, which is the floor Sonarr uses and
the floor connection testing enforces.

## Connection and scope

Configure the endpoint with **host**, **port** (default 6800), **rpc_path**
(default /rpc), and **use_ssl**. Set **secret_token** to the token the daemon
was started with via `--rpc-secret`; the plugin sends it as aria2's `token:`
first parameter on every call. aria2's older `--rpc-user` / `--rpc-passwd`
HTTP basic authentication is deprecated upstream and is not supported here.

**directory** is the fallback download directory; a directory Scryer routes for
an individual download takes precedence and is sent as aria2's per-download
`dir` option.

The plugin lists aria2's active, waiting, and stopped results and reports the
content path from aria2's file list — the file itself for a single-file
download, the longest shared directory otherwise. aria2 must therefore retain
the completed result and expose a path Scryer can reach for import. Listings
ask aria2 for only the status members the plugin actually reads, so the
per-torrent piece `bitfield` is not transferred on every poll.

## Identity

An item's id is its BitTorrent info hash whenever one can exist. aria2's
`addUri` / `addTorrent` answer only with a GID, and for a magnet or a torrent
URL that GID belongs to the metadata download aria2 throws away once the
metainfo resolves — so the plugin derives the hash itself, from the release
Scryer supplies, from the magnet's `btih` (hex or base32), or by hashing the
bencoded `info` dictionary of the torrent bytes. When aria2 already knows the
hash it is read back from `tellStatus`, following `followedBy` to the payload
download. Only a source that can have no info hash at all falls back to the
GID.

Metadata downloads are hidden from the queue while they resolve, the way
Sonarr hides them; the download appears under its info hash as soon as aria2
has the metainfo.

## What Scryer can do

It submits magnet URIs, torrent URLs, torrent files, and torrent bytes using
`addUri` or `addTorrent`. Directory isolation is supported, along with pause,
resume, and removal. A Scryer seeding goal is passed through to aria2's own
per-download `seed-ratio` and `seed-time` options, so aria2 stops seeding on
the same terms Scryer would; aria2 stops seeding but never removes the entry
itself. There are no queue-priority, start-paused, or force-start controls.

States are reported from aria2's own status plus the fields it publishes
alongside it: a torrent aria2 keeps `active` after its data is complete (or one
it flags with `seeder`) is reported as **seeding** rather than merely
completed, and a download being hash-checked (`verifiedLength`) is reported as
**verifying** and is not offered for a move until the check is done. A status
aria2 does not document today keeps polling rather than failing the download.

`can_remove` is reported honestly: true once aria2 itself has stopped the
torrent (`complete`), false while data is still missing, and unknown while
aria2 is still seeding — aria2's `tellStatus` does not publish the seed-ratio
or seed-time goal, so only Scryer can decide. The share ratio is reported
against what has actually been downloaded so far, which is the ratio a tracker
is counting mid-download.

## Removal and files on disk

aria2 has no call that deletes downloaded files (aria2 issue #728). The plugin
declares that, and Scryer therefore removes only the download entry: **the
downloaded files are left on disk** for you to clean up. This is also reported
as a client warning.

## Post-import

There is no plugin-side post-import mutation. aria2 has no label, tag,
category or view to write back to, so an import recorded by Scryer neither
relabels nor removes the download, and retention stays aria2's own
configuration plus Scryer's seeding gate.

The earlier **post_import_action** option is retired. It offered a "remove
result" that called aria2's `removeDownloadResult` after an import; removing a
finished download is Scryer's decision through the seeding gate, not the
client plugin's, and Sonarr's aria2 client has no post-import action either. A
stored value of `retain` or `remove` is simply no longer read — both are
no-ops, and no configuration needs changing.

## Connection testing

Testing calls `aria2.getVersion` and rejects a daemon older than 1.34.0,
naming both the required and the reported version. Failures are reported with
the reason: a rejected secret token (aria2 answers an XML-RPC fault whose text
is `Unauthorized`) as an authentication failure, a redirect or a missing RPC
path as a configuration error, a refused or untrusted connection as an
unavailable upstream, and a 5xx or a timeout as temporary.
