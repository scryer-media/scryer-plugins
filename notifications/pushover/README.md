# Pushover

Push notifications through the [Pushover Messages API](https://pushover.net/api).
The channel sends one `POST https://api.pushover.net/1/messages.json` per event,
with a per-event title and body built from Scryer's notification contract,
optional device targeting, emergency-priority repeats, a sound, a time-to-live,
a tappable link to the title's metadata page, and Pushover's end-to-end
encrypted message format.

## Configuration

| Setting | Type | Required | Purpose |
| --- | --- | --- | --- |
| **api_key** | Password | Yes | The Pushover application token, from <https://pushover.net/apps>. |
| **user_key** | Password | Yes | The Pushover user or group key that receives the notification. |
| **devices** | Tag | No | Device names to target; empty means every device on the account. Each name is at most 25 characters of letters, digits, underscores and hyphens. |
| **priority** | Select | No | `-2` Silent, `-1` Quiet, `0` Normal (default), `1` High, `2` Emergency. |
| **retry** | Number | No | Emergency only: seconds between repeats, 30–10800. |
| **expire** | Number | No | Emergency only: how long to keep repeating until acknowledged, 30–10800 seconds. |
| **ttl** | Number | No | Seconds after which Pushover deletes the message from the user's devices. `0` keeps it. |
| **sound** | String | No | A Pushover sound identifier or a custom sound you uploaded; empty uses the device default. See <https://pushover.net/api#sounds>. |
| **metadata_link** | Select | No | Which metadata site the notification's link opens: `auto` (default), `none`, or a specific site. |
| **encryption_key** | Password | No | A 64-character hexadecimal key matching the one set in the Pushover app, for end-to-end encryption. |

Configuration problems are reported as configuration problems, naming the
setting — a priority outside `-2..2`, a non-numeric `retry`, an emergency
priority with no usable retry/expire window, a negative `ttl`, a device name
Pushover cannot accept, an unknown `metadata_link`, or an encryption key that is
not 64 hex characters. The plugin does not report them as failed deliveries.

Device names are checked strictly during a connection test and leniently on a
live send: a name outside Pushover's documented character set fails the test, but
a running channel keeps delivering and records a warning instead.

## What is sent

* **title** — the event heading Scryer composed ("Grabbed: Example Show",
  "Import complete: Example Show", "Download failed: Example Show"), trimmed to
  Pushover's 250-character limit.
* **message** — the event summary, followed by the facts the event carries:
  episode, quality, release and release group, indexer, size, download client,
  destination or source path, health check and detail, application versions,
  subtitle languages, or media-request status. Trimmed to Pushover's
  1024-character limit, dropping detail lines from the end before the summary.
* **timestamp** — the moment the event happened, from the contract's
  `occurred_at`, so a delayed delivery still shows the right time.
* **url** / **url_title** — a tap target. A manual-interaction event links into
  Scryer; everything else links to the title's page on the site chosen by
  `metadata_link`, or under `auto` to the best identifier the title actually
  carries for its library type (TVDb, TVMaze, IMDb, TMDb, AniDB for episodic
  libraries; TMDb, IMDb, TVDb otherwise). A site the title has no id for renders
  no link rather than a dead one.
* **priority**, **device**, **sound**, **ttl** — as configured. `retry` and
  `expire` are sent only for emergency priority.

Truncation and other degraded rendering is reported through the delivery
response's warnings, which Scryer logs; the notification is still sent.

`Download` events are failures, not imports: Scryer maps a failed download onto
that event type, so the channel renders the client and its status and never a
destination path.

## Delivery outcomes

| Pushover says | Scryer sees |
| --- | --- |
| `200` with `status: 1` | Delivered. The `receipt` (emergency) or `request` id is recorded as the delivery id. |
| `{"token":"invalid"}` | An authentication failure naming **api_key**. |
| `{"user":"invalid"}` | A configuration error naming **user_key** (invalid key, or no active devices). |
| A rejected `device`, `sound`, `retry`, `expire`, `ttl` or `priority` | A configuration error naming that setting. |
| A rejected message, title or url | A permanent error: the message this plugin built was wrong. |
| Any other `4xx` | A configuration error carrying Pushover's own `errors` text. Pushover documents every 4xx as permanent. |
| `429` | A delivery failure with the seconds until the monthly quota resets, taken from `X-Limit-App-Reset`. |
| `5xx` | A delivery failure with Pushover's documented five-second retry floor. |

When Pushover reports that the account's monthly message allowance is nearly
exhausted (`X-Limit-App-Remaining` at or below 5% of `X-Limit-App-Limit`), the
channel adds a warning to the delivery so the operator hears about it before
messages start being refused. Since 1 May 2026 that allowance is per **account**
and shared by every Pushover application registered to it, so a warning here is
about the whole account, not just this channel.

## Encrypted delivery

With **encryption_key** set, `title`, `message` and `url` are gzipped,
encrypted with AES-256-CBC (PKCS7) under a fresh random 16-byte IV,
authenticated with HMAC-SHA256 over IV‖ciphertext, and sent as base64 of
IV‖ciphertext‖MAC with `encrypted=1` — the format the Pushover apps decrypt
with the same key (<https://pushover.net/api#e2ee>). The application token and
user key are never encrypted.

Because Pushover cannot measure a field it cannot read, the API's length limits
are applied to the encoded value: title and message are shortened until the
ciphertext fits, and `url_title` is omitted entirely, since any encrypted field
is at least 108 characters and `url_title` is limited to 100. Both are reported
as warnings.

## Deliberate limits

* Messages are plain text. Pushover's `html=1` and `monospace=1` exist, but the
  1024-character limit is measured on what the API receives, so every markup
  byte is content lost.
* The configured priority is used as-is. Pushover's priority has real delivery
  semantics — `2` needs a retry/expire window and `-2` suppresses the
  notification entirely — so the channel never raises or lowers it based on an
  event's severity.
* Image attachments are not sent. Pushover accepts `attachment_base64`, but
  Scryer's contract carries a poster *URL* rather than bytes, and fetching it
  would need egress to an arbitrary image host that the plugin's allowed-hosts
  list cannot express.
* The plugin never creates users, groups, devices, applications, or custom
  sounds.
