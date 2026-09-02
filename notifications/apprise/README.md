# Apprise

Send Scryer notifications through a self-hosted
[Apprise API](https://github.com/caronc/apprise-api) server, which fans them out
to any of the services [Apprise](https://github.com/caronc/apprise) supports.
The channel sends one `POST` per event — `/notify/<key>` for a configuration
stored on the server, or `/notify` with a URL list for the stateless mode — with
a per-event title and body built from Scryer's notification contract, an Apprise
notification type, optional tags, and an optional poster attachment.

This plugin talks to the API server only. It does not run the Apprise CLI, read
local Apprise configuration files, or touch the filesystem.

## Configuration

| Setting | Type | Required | Purpose |
| --- | --- | --- | --- |
| **server_url** | URL | Yes | The Apprise API base URL, for example `http://apprise.example:8000`. |
| **configuration_key** | String | One of | A configuration stored on the server; sends `POST /notify/<key>`. Letters, digits, `_` and `-`, up to 128 characters. |
| **stateless_urls** | Multiline | One of | Apprise destination URLs sent with every notification; sends `POST /notify`. One per line or comma separated. |
| **notification_type** | Select | No | `info` (default), `success`, `warning`, `failure`, or `auto`. |
| **tags** | Tag | No | Apprise tags selecting which of the configuration's URLs to notify. A space means AND, a comma means OR. Configuration key only. |
| **include_poster** | Bool | No | Sends the title's poster URL as an Apprise attachment. |
| **auth_username**, **auth_password** | String / Password | No | HTTP Basic credentials for a reverse proxy in front of the API. Apprise itself has no authentication. |

`configuration_key` and `stateless_urls` are mutually exclusive and exactly one
is required.

Configuration problems are reported as configuration problems, naming the
setting — a `server_url` that is not an absolute `http(s)` URL, both routes set
or neither, a key outside the character set the API routes, an unparseable tag,
an unknown notification type, or a `stateless_urls` entry that is not an Apprise
URL. The plugin does not report them as failed deliveries.

Two rules are strict during a connection test and lenient on a live send,
because losing a notification is worse than delivering it slightly wrong:

* **tags with stateless URLs** fail the test and are dropped with a warning on a
  live send. A stateless notification has no tagged configuration for a tag to
  match, so any tag but Apprise's implicit `all` selects nothing;
* **a `stateless_urls` entry with no `scheme://`** fails the test and is left in
  place with a warning on a live send, so the entries the server *can* parse are
  still notified.

An invalid **tags** value is an error on every send rather than a warning:
dropping it would fall back to `all` and notify every URL behind the key, which
is the one outcome an operator using tags is trying to avoid.

## Notification type

`info`, `success`, `warning` and `failure` are sent for every event, which is
what Sonarr does. `auto` derives Apprise's type from the event instead:

| Event | Type |
| --- | --- |
| Severity `error` — failed download, rejected import, subtitle search failed | `failure` |
| Severity `warning` — health issue | `warning` |
| Manual interaction required | `warning` |
| Import complete, upgrade, post-processing complete, title added, subtitle downloaded, media request approved, health restored | `success` |
| Everything else — grab, rename, deletions, tests | `info` |

The default stays `info`, so an existing channel keeps behaving exactly as it
did.

## What is sent

* **title** — the event heading Scryer composed ("Grabbed: Example Show",
  "Import complete: Example Show", "Download failed: Example Show").
* **body** — the event summary, followed by the facts the event carries:
  episode, quality, release and release group, indexer, size, download client,
  destination or source path, deleted file, health check and detail,
  application versions, subtitle languages, or media-request status. Apprise
  requires a body, so an event with no summary falls back to its heading.
* **type** — as configured, or derived under `auto`.
* **format** — always `text`. The body is plain text and says so, rather than
  relying on the server's default staying `text`; Apprise converts it for each
  destination service.
* **tag** — the configured tags, joined with commas, for a configuration key.
* **urls** — the normalised stateless URL list, for the stateless route.
* **attachment** — the title's poster URL when **include_poster** is on. The
  Apprise **server** downloads it, so a relative or non-`http(s)` poster is
  dropped with a warning rather than sent. A connection test with no poster
  attaches the Scryer logo instead, so the attachment path is exercised end to
  end.
* **X-Apprise-ID** — the event's own id, so a notification is traceable from
  Scryer's log to the Apprise server's.

`Download` events are failures, not imports: Scryer maps a failed download onto
that event type, so the channel renders the client and its status and never a
destination path.

Nothing is truncated. Apprise documents no length limit and applies each
destination service's own limits itself.

## Delivery outcomes

The channel asks for `Accept: application/json`, which is the only way the API
answers with its `{"error", "details"}` body — the one place it says *why*. The
`details` log records are mined for `WARNING`/`ERROR`/`CRITICAL` lines, so on a
partial failure the operator sees which service refused.

| Apprise says | Scryer sees |
| --- | --- |
| `200` | Delivered. Any warnings the server logged are attached to the delivery. |
| `204` | A configuration error. **This is the outcome Sonarr cannot see:** `204` means the key names a configuration the server has never stored, or the stateless list produced no usable URL — nothing was notified — and because `204` is a 2xx, Sonarr reports both the send and the connection test as successful. |
| `424` | A delivery failure: at least one destination refused, quoting the server's own log lines. May be partial; the API returns one status for the whole fan-out. |
| `400`, `405`, `406`, `431` | A permanent error — the request this plugin built was rejected. |
| `401`, `403` | An authentication failure naming **auth_username**. Apprise has no authentication of its own, so this is always the reverse proxy. |
| `404` | A configuration error naming **server_url** and the endpoint that was missing. |
| `429` | A delivery failure, with `Retry-After` if the proxy sent one. |
| `5xx` | A delivery failure, with `Retry-After` if present. |
| Any other non-2xx with a body that is not the API's JSON | A configuration error naming **server_url**: something that is not apprise-api answered. |
| A 2xx with a body that is not the API's JSON | Delivered, with a warning — the message may well have arrived. |

Every outcome records a `target_results` entry naming the route. For the
stateless mode only the URL *schemes* are recorded, never the URLs themselves,
which routinely carry credentials.

## Connection test

The test sends a real notification, as Sonarr's does, and adds two
unauthenticated probes whose findings are **warnings only** — a probe that
cannot decide must never stop a delivery:

* `GET /status` — warns when the server does not answer like apprise-api, when
  it reports itself unhealthy (`417`), when `APPRISE_STATEFUL_MODE=disabled`
  makes a **configuration_key** a dead end, and when `APPRISE_ATTACH_SIZE=0` or
  an attachment lock means **include_poster** will be refused.
* `GET /json/urls/<key>` — only with a configuration key and configured tags.
  Warns for each tag no URL in the stored configuration carries, and when the
  stored configuration is empty. A tag that matches nothing is the failure mode
  where the channel looks healthy and silently notifies no one.

## Deliberate divergences from Sonarr

* **The configuration key charset is the server's, not Sonarr's.** Sonarr
  rejects anything outside `^[a-z0-9-]*$`; the API routes `/notify/<key>` as
  `[\w_-]{1,128}`, so `MyKey` and `home_lab` are accepted here.
* **`tags` is a `Tag` field**, matching Sonarr's own field type. The stored
  value is the same comma-separated text the previous `String` field held.
* **Per-event detail lines and the `auto` type** have no Sonarr equivalent:
  Sonarr hands its proxy one prose sentence and one fixed type.
* **Event subscriptions are a Scryer channel setting**, not per-event
  checkboxes on the plugin.
