# Synology Download Station

This plugin speaks Synology's Download Station task API, discovering the
available API versions once per hour and retaining its authenticated DSM
session between calls. One Scryer client covers both of the task types Sonarr
splits into two clients: BT (torrent) and NZB (usenet).

## Connection and configuration

Set **host** (default 127.0.0.1), **port** (default 5000), and **use_ssl** for
the DSM endpoint, plus an account in **username** and **password** that can
create and control Download Station tasks, and that has access to FileStation.

**category** is a sub-folder of Download Station's default destination —
letters and `-` only, with an optional leading dot — and is also the folder
Scryer treats as its own when it reads the task list. **directory** is a
destination relative to a Download Station shared folder, without a leading
slash. The two are mutually exclusive, exactly as in Sonarr; connection testing
rejects the combination. If both are set anyway, the directory wins for both
adding and polling, so nothing is misrouted.

Per-download routing (`routing.download_directory`) overrides the configured
destination, provided the routed directory stays inside the configured
directory or still contains the configured category. A routed directory outside
that scope is refused at add time, because the task list is filtered on the same
scope and Scryer would otherwise never see the download again.

2-Factor Authentication on the DSM account prevents any API login; Download
Station cannot be used from Scryer with it enabled.

## Behaviour and limits

The plugin accepts magnets, torrent URLs, torrent bytes and NZB bytes or URLs,
and reports queued, active and completed tasks. Task destinations are resolved
through `SYNO.FileStation.List` from their shared-folder-relative form
(`downloads/tv`) to the real path on the NAS (`/volume1/downloads/tv`), cached
per DiskStation serial number and shared folder, so remote path mappings can be
written against paths that actually exist. Download ids are prefixed with the
SHA-1 of the DiskStation serial number, which is what keeps them stable across
DSM restarts; a client that cannot report its serial number fails the call
rather than re-keying every tracked download.

Removal is supported; removal *with data* is not, because Download Station's
`delete` API has no delete-data flag — Scryer removes the payload itself.
Pause, resume and force-start are not exposed by this client. Download Station
reports no per-torrent seeding goal, so `can_remove` is reported honestly: true
once Download Station itself has stopped seeding a torrent, false while it is
still seeding, and unknown when the task is complete but paused. An NZB task has
no seeding obligation and is removable as soon as its data is complete.

There is no plugin-side post-import mutation: Download Station has no per-task
label, tag or category to write back to, so an import recorded by Scryer neither
relabels nor removes the task, and retention stays the NAS administrator's
policy (and Scryer's seeding gate).

## Connection testing

Testing validates the settings, authenticates, checks that the Download Station
Task API supports version 2, verifies that the download destination exists and
is a folder inside an existing shared folder, and then performs a full task
listing — the same four checks Sonarr runs.
