# Kodi

Shows Kodi's on-screen notification and keeps its video library in step, over
Kodi's JSON-RPC API (`POST {server_url}/jsonrpc`). GUI notification, library scan
and library clean are independently configurable.

This channel used to ship as **`xbmc`**. XBMC became Kodi in 2014, so the plugin
id, provider type and display name are now `kodi`; `xbmc` remains a provider
alias, and every configuration key is unchanged, so an existing channel keeps
working without being touched.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **server_url** | Effectively yes | Kodi's web server, for example `http://kodi.local:8080`. |
| **host**, **port**, **use_ssl** | No (legacy) | Superseded by `server_url`; used only when it is empty. |
| **url_base** | No | Path of the JSON-RPC endpoint, default `/jsonrpc`. Applied to `server_url` only when that URL has no path of its own. |
| **username**, **password** | No | Kodi web-server HTTP Basic credentials (Settings → Services → Control), not a Kodi profile. |
| **display_time** | No | Seconds the toast stays up. Default 5, minimum 2. |
| **notify** | No | Show the on-screen notification. Off by default. A connection test always shows one. |
| **notification_poster** | No | Use the title's poster as the notification icon when the event carries one. Off by default. |
| **update_library** | No | Run `VideoLibrary.Scan` after media changes. |
| **clean_library** | No | Run `VideoLibrary.Clean` after events that removed or replaced files. |
| **always_update** | No | Do library work even while Kodi is playing video. |
| **show_dialogs** | No | Let Kodi show its scan/clean progress dialog. Off by default. |

Kodi must have **Settings → Services → Control → Allow remote control via HTTP**
enabled. If "Require authentication" is on there, put those credentials in
`username`/`password`.

### Why `server_url` matters

Scryer builds each plugin's HTTP allowlist from its descriptor plus the
configuration values that parse as URLs. This channel declares no static host, so
`server_url` is what puts Kodi's origin on the allowlist — a bare `host` never
parses as a URL and leaves the allowlist empty, which denies every request. The
legacy `host`/`port`/`use_ssl` settings are still read so existing configurations
keep their values, and the channel warns, naming the exact URL to paste, when it
had to fall back to them. If Scryer refuses the request, the error says exactly
this and names the setting to fix.

## What each event does

| Event | GUI notification | `VideoLibrary.Scan` | `VideoLibrary.Clean` |
| --- | --- | --- | --- |
| Grab | yes | — | — |
| Download (a **failed** download) | yes | — | — |
| Import complete | yes | yes | only when the import replaced a file |
| Upgrade | yes | yes | yes |
| Rename | — | yes | yes |
| File deleted / deleted for upgrade | yes | yes | yes |
| Title added | yes | yes | yes |
| Title deleted | yes | yes | only when the event carries deleted paths |
| Import rejected, post-processing, subtitles, media requests, health, application update, manual interaction | yes | — | — |
| Test | always | — | — |

The scan and clean columns are further gated by `update_library` and
`clean_library`; the notification column by `notify`, except on a connection test.

Two of these deliberately differ from Sonarr. Scryer's `Download` event carries a
**failed** download, not a completed one, so it is headed "Download Failed" and
never touches the library; Sonarr's "episode downloaded" behaviour lives on
`ImportComplete` and `Upgrade` instead. And Sonarr acts on a series delete only
when it deleted files, which the Scryer contract does not carry — the deleted
paths on the event stand in for that flag.

## Library work

Before any scan or clean the channel asks `Player.GetActivePlayers` once. If
video is playing it skips both and says so in the response warnings, unless
`always_update` is on.

The scan is scoped to the title's own folder **as Kodi knows it**, which is what
makes a scan seconds rather than minutes. Series are looked up through
`VideoLibrary.GetTVShows` and movies through `VideoLibrary.GetMovies`, matching
in this order:

* series — `uniqueid.tvdb`, then the TVDB id in `imdbnumber` (Kodi's TVDB scraper
  writes it there, and it is the only identity Sonarr reads), then
  `uniqueid.imdb`/`imdbnumber`, then `uniqueid.tmdb`, then the label;
* movies — `uniqueid.tmdb`, then `uniqueid.imdb`/`imdbnumber` (the `tt` prefix is
  normalised), then `uniqueid.tvdb`, then the label narrowed by year.

A series' `file` is already the show folder; a movie's is the movie file, so its
parent directory is used. Kodi's virtual paths (`stack://`, `multipath://`,
archive schemes) are not directories and are not sent. When nothing matches, the
whole video library is scanned — Sonarr's behaviour — and the response carries a
warning saying so.

The clean is narrowed to the half of the library the event touched
(`content: movies`/`tvshows`) and, where Kodi supports it, to the same folder.
Both parameters are version-gated: `content` needs JSON-RPC 10 (Kodi 18 Leia) and
`directory` needs JSON-RPC 12 (Kodi 19 Matrix). On anything older, and whenever
the version is unknown, the clean is the unscoped one Sonarr sends. An anime
title can be either a movie or a series in Scryer, so its clean stays unscoped by
content.

## The notification

The title is the event's branded header — `Scryer - Grabbed`, `Scryer - Imported`,
`Scryer - Deleted`, and so on — and the body is the event summary with the
quality appended when the summary does not already name it. Newlines are
collapsed and text over 256 characters is truncated with an ellipsis and a
warning, because a Kodi toast clips.

The icon is Kodi's own `info`, `warning` or `error` badge, chosen from the
severity Scryer stamps on the event. Turning on `notification_poster` sends the
title's poster URL instead, when the event carries one; Kodi downloads it, so
leave it off if Kodi has no internet access.

## Failures

Every failure names the setting that caused it, on ordinary sends as well as on
Test:

| Response | Result |
| --- | --- |
| `2xx` with a `result` | Delivered. |
| `2xx` with a JSON-RPC `error` | `Permanent` — "Kodi JSON error. Code = N, Message: … (method …)". |
| `401`/`403` | `AuthFailed` — the Kodi web-server `username`/`password`. |
| `404` | `InvalidConfig` — `url_base`, or the path in `server_url`. |
| a body that is not JSON | `InvalidConfig` — the URL points at Kodi's web *interface*, not its JSON-RPC endpoint. |
| `429`/`5xx` | Delivery failure, carrying `Retry-After` when present. |
| other non-2xx | `InvalidConfig`, quoting what Kodi said. |
| egress refused by Scryer | `InvalidConfig` — set `server_url`. |
| Kodi unreachable | `UpstreamUnavailable` on a connection test; a delivery failure on a live send, so a network blink is never reported as a broken setting. |

Each JSON-RPC call is reported separately in `target_results`, keyed by method,
so a toast that worked and a clean that did not are both visible. When every call
fails for the same configuration reason, the whole send is reported on the typed
error lane instead.

A connection test always calls `JSONRPC.Version` first — it is the cheapest proof
that the address and the credentials are right — and then shows the notification
whether or not `notify` is on, so the operator can see it worked. The version it
reports is echoed back as a warning, along with a note when the Kodi is old
enough to lose `uniqueid` matching or the scoped clean.

## Limits

Kodi add-ons, remote file copying and notification attachments are not supported.
Library updates are not batched: Sonarr coalesces them per host so that a season
import is one scan, and Scryer's core has no equivalent for a plugin channel
today, so an import of many files produces one scan per event.
