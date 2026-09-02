# Transmission

This plugin talks to Transmission's RPC endpoint, including the required
session-ID handshake (the session id is cached in plugin state, so the 409
handshake is paid once per client, not once per call). Vuze/Azureus expose the
same RPC surface and reach this plugin through its `vuze` and `azureus`
provider aliases.

## Connection and configuration

Set **host**, **port** (default 9091), **use_ssl**, and **url_base** (default
`/transmission/`), plus optional **username** and **password**.

**category** defaults to `scryer-tv`. On Transmission 4.0 or newer it is
applied as a label and is what scopes Scryer's queue; on older releases, which
have no labels, it is used as a sub-folder of Transmission's download directory
and the queue is scoped by path instead. Allowed characters are `a-z` and `-`,
with an optional leading dot.

**directory** forces a destination for everything Scryer adds and cannot be
combined with **category** — the two describe different sets of torrents, so a
client configured with both is refused at add time and by *Test*. Scryer can
also route an individual download to its own directory, which is unaffected.

**recent_priority**, **older_priority** and **add_paused** control queue
placement and initial state. An explicit per-download queue placement from
Scryer wins over both.

**post_import_category** is the label applied once Scryer has imported a
download, and **label_after_import** (default on) is the switch for it.

## Version gate

Labels arrived with Transmission 4.0. This plugin reads the version from
`session-get` once per client and caches it, then gates all three label uses on
it: the `labels` argument on `torrent-add`, the post-import label, and
label-based queue scoping. Below 4.0 it falls back to the directory rules
above, and *Test* reports the version it found. *Test* requires Transmission
2.40 or newer, or — for Vuze/Azureus, which answer with a protocol version
instead — RPC protocol 14 or newer.

## Behaviour and limits

Magnets and torrent URLs, files, or bytes are supported. The adapter can
isolate work by directory, tag, or category, pause/resume it, remove it with or
without data, and use Transmission's seed-ratio and idle-seeding controls. It
does not advertise force-start.

Transmission has no total-seed-time limit, so a seed-time goal is mapped onto
the per-torrent **idle** limit (`seedIdleLimit`, mode 1), which is what Sonarr
does as well. The client therefore reports `supports_seed_time_limit`, but the
limit it enforces is an idle limit: a torrent nobody is downloading from
reaches it sooner than a wall-clock seed-time goal would suggest.

Queue items report Transmission's own view: the states include `Paused` for a
stopped incomplete torrent, `Verifying` while it is being checked and `Seeding`
for a finished torrent Transmission is actively seeding, plus transfer rates,
the completion time from `doneDate`, and `is_private` when Transmission reports
it. `can_remove` is tri-state and reflects only what Transmission's *own*
limits prove; `can_move_files` says whether the data is complete on disk.
Whether a completed torrent is actually removed is Scryer's decision through
its seeding policy — this plugin never removes anything on its own.

## Post-import

After a successful import Scryer asks the plugin to mark the download. This
plugin swaps labels and nothing else: the label that scoped the download (the
one Scryer routed, or the configured category) is dropped and
**post_import_category** is added, case-insensitively and preserving
Transmission's own casing for every other label. A torrent that is no longer in
the client is logged and treated as done. Nothing is removed and no data is
deleted.

The older `post_import_action` setting is retired. Its `remove` and
`remove_with_data` values asked the plugin to delete a still-seeding torrent at
import time, which Scryer's seeding policy exists to prevent. Existing
configurations still parse: `retain` is read as "leave the labels alone",
anything else as "apply the label". Use **label_after_import** from now on.
