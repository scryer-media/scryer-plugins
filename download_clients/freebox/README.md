# Freebox Download

This plugin controls the BitTorrent portion of the Freebox OS download
manager. It authenticates with Freebox's challenge-response API and retains
the resulting session token in plugin state; a normal HTTP username/password
login is not used.

## Connection and configuration

Set **host** (default mafreebox.freebox.fr), **port** (default 443),
**api_url** (default /api/v1/), and **use_ssl**. Freebox OS requires HTTPS and
is removing plain HTTP, so leaving SSL on is the supported configuration; the
client status reports a warning when it is off. The credentials are the
Freebox application pair: **app_id** and **app_token**, created by authorising
an application from the Freebox front panel. The application must hold the
*Downloader* permission — the plugin checks the permission set the box returns
when the session opens and fails the connection test with a clear message
instead of an opaque `insufficient_rights` on the first listing.

**destination_directory** overrides Freebox's configured download directory.
With no explicit destination, **category** is appended to the Freebox default
directory and also scopes polling. The two are mutually exclusive, a category
may contain only letters and `-`, a destination must be an absolute Freebox
path, and **api_url** must be a path (`/api/v1/`), not a full URL. All of these
are checked by the connection test, and the ones the box would reject are
checked again before an add is sent. If category and destination are both set
anyway, the destination wins for both adding and polling. The test
also reads the box's own `/api_version` and refuses an **api_url** that asks
for a newer API than the Freebox serves.

**recent_priority** and **older_priority** can be `first`, which puts the task
at the head of the Freebox queue; **add_paused** adds tasks stopped. An
explicit queue placement or initial state that Scryer routes for a single grab
overrides both settings.

## Job folders and imports

Every download is given its own folder: the destination (or the default
directory plus the category) with the cleaned release title appended, which is
what the task's `download_dir` becomes. Scryer therefore imports from a folder
holding exactly one release rather than scanning the whole download directory.
When Scryer routes an isolated directory for a grab, that path is used exactly
as supplied.

The reported output root is the destination (or default directory plus
category) *without* the per-release folder, which is what path mapping expects.

## Behavior and limits

The plugin accepts magnets and torrent URLs, files, or bytes. It supports
directory isolation, per-request directories, queue placement, add-paused, a
torrent seed-ratio limit (sent as Freebox's integer `stop_ratio` percentage),
pause and resume, and removal with or without data. Freebox has no tags or
labels, no seed-time limit and nothing that force-starts a task, so those are
declared unsupported.

Task states are reported at Freebox's own resolution: `checking` as verifying,
`repairing` as repairing, `extracting` as extracting and `seeding` as seeding,
with `error` carrying the documented error description. A status this plugin
does not recognise keeps polling as downloading and says so in the item
message. Freebox keeps no separate failed history, so only the live task list
is polled — once per queue refresh.

Freebox tasks are not changed after a Scryer import. Freebox OS has no tag,
label, category or view to write back to, so there is no imported label and no
cleanup policy: keeping or deleting the data remains an explicit Scryer control
request or a Freebox policy decision, and the client reports that it never
removes completed downloads on its own.

## Errors

Failures carry the code that matches them rather than a generic retry:
unreachable host or TLS failure as upstream-unavailable, 401/403 and the
documented login error codes as authentication failures, a 404 or a redirect to
a login page as a configuration problem naming *API URL*,
`too_many_tasks`/`hibernating`/`out_of_memory` as temporary, and
`task_not_found`/`invalid_url`/`invalid_file` as permanent. A session token
expires by design, so an `auth_required` is retried once with a fresh token
before it is reported. An add the box refuses with `exists` adopts the task
that already exists instead of failing the grab, and removing a task that is
already gone succeeds.
