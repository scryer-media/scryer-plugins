# Deluge

This plugin drives Deluge through its Web UI JSON-RPC API (`/json`). It manages only
torrents carrying the configured label, so an existing Deluge deployment can coexist
with other work without Scryer treating every torrent as its own.

Deluge **2.x** is the target. The 1.3 fallbacks are kept where Sonarr keeps them:
`daemon.info` when `daemon.get_version` is missing, and status keys a 1.3 daemon does
not publish (`completed_time`, `private`, `num_files`) are treated as absent rather
than required.

## Connection and configuration

Set **host**, **port** (default 8112), optional **url_base**, and **use_ssl** for the
Web UI endpoint. **password** is the Deluge Web password (default `deluge`), not a
daemon credential. The plugin caches its Web session cookie in plugin state and
re-authenticates on Deluge's "not authenticated" JSON-RPC codes (1 and 2), which also
reconnects the Web UI to its daemon.

**category** defaults to `scryer-tv`. It is a Deluge *label*: it scopes the poll, and
it is applied to every torrent Scryer adds. **post_import_category** is the label moved
onto a torrent once Scryer has imported it. Deluge's Label plugin lower-cases label ids
and allows only `a-z`, `0-9` and `-`; both fields are validated against that rule during
**Test**, and the values sent to Deluge are lower-cased.

Optional **download_directory** and **completed_directory** are passed to Deluge as
`download_location` and `move_completed_path` + `move_completed`, and are also what the
client reports as its output roots. **add_paused**, **recent_priority**, and
**older_priority** control the initial torrent state and queue position.

Scryer's per-download routing wins over the configured values: a routed directory
replaces `download_directory` for that grab, and a routed category/tag/label replaces
the configured category.

### Label plugin

A configured category or post-import category requires Deluge's **Label** plugin
(Preferences > Plugins > Label). **Test** fails with a configuration error naming the
field when it is not active, because a torrent that cannot be labelled is invisible to
the label-filtered poll this client runs.

Missing labels are created rather than assumed: **Test** creates and re-checks them, and
the add and post-import paths create the label they are about to use if this instance
has not already seen it. That is more than Sonarr does — Sonarr only checks at test
time — and it means changing the category in Scryer does not silently break grabs.

## Behavior and limits

Magnets, core-supplied `.torrent` bytes, torrent files and torrent URLs are all
accepted. The preference order is magnet, then bytes, then file, then URL — magnet
first because that is what Sonarr does for clients that do not prefer torrent files,
and URL last because Scryer's core fetches `.torrent` bytes with the indexer's own
credentials while a plugin-side GET has no indexer cookies and would be rejected by
most private trackers. The plain GET remains only as a last resort for a public,
cookieless URL.

Scryer can route a torrent by label/category or directory, remove it (with or without
data), move it to the top of the queue, and set a seed **ratio** goal (`stop_ratio` +
`stop_at_ratio`). Deluge has no seed-*time* limit, so `supports_seed_time_limit` is
false and a time goal stays a Scryer-side policy. The descriptor does not advertise
pause or resume, even though paused state is reported: Deluge's
`core.pause_torrent`/`core.resume_torrent` argument shape differs across 1.3, 2.0 and
2.1 and has not been verified against a live daemon.

Deluge keeps no separate failed-download history — an errored torrent stays in the same
listing with `state == "Error"` — so the history call returns nothing and each poll is a
single `web.update_ui`.

Torrents without a hash or name are skipped. The first time that happens the plugin
reconnects Deluge's Web UI to its daemon (that is usually what a hashless listing
means); if it happens again it warns instead, and `status` carries a warning naming the
count.

### What Scryer reports

- `can_move_files` is about the data only: true once the payload is complete and
  nothing is being verified or moved.
- `can_remove` is tri-state and is the *client's* verdict on its seeding obligation:
  true only when Deluge itself has paused an auto-managed torrent at its `stop_ratio`,
  false while a client-side ratio goal is provably unmet, and unknown when Deluge has no
  ratio goal to enforce — Scryer's own seeding policy decides then.
- `is_private` is reported only when Deluge reports it; it is never inferred.
- Status mapping follows Sonarr's table (`Paused`/`Queued`/`Downloading`/`Seeding`,
  finished torrents Completed) with two Scryer refinements: `Checking` is reported as
  *Verifying* and `Moving` as *ImportPending*, so nothing reads files out from under a
  relocation. An unrecognised state keeps polling as Downloading; it is never treated
  as a fault.
- `completed_at`, upload/download rates, uploaded bytes, and the label in Deluge's own
  casing are reported when the daemon publishes them.

### Output roots

`status` reports, in order: **completed_directory** and **download_directory** when
they are configured; otherwise the label's `move_completed_path` (when the label
applies its own move-completed setting), otherwise the daemon's `move_completed_path`
if `move_completed` is on, otherwise its `download_location`. Scryer's core does the
remote-to-local path mapping from those roots, so a Deluge with no directories
configured in Scryer still reports one.

### After an import

The post-import handoff is **non-destructive**. When a post-import category is
configured (and differs from the grab category), or the core routes one, the label is
created if needed and moved onto the torrent with `label.set_torrent`. Deluge allows one
label per torrent, so this implicitly drops the grab-time label. A refusal is logged as
a warning and the import still succeeds, exactly as Sonarr does.

The plugin never removes a torrent at import time, and `status` reports
`removes_completed_downloads: false`. Removal of a finished torrent is Scryer's decision
through its seeding gate. The old `post_import_action` option (`retain` / `remove` /
`remove_with_data`) has been retired; a config that still carries any of its values
keeps working and now only relabels.
