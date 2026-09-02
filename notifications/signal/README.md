# Signal

Sends Scryer notifications through a [signal-cli-rest-api](https://github.com/bbernhard/signal-cli-rest-api)
server (`POST /v2/send`). The plugin never talks to Signal directly: the server
owns the registered account, and this channel is one of its API clients.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **server_url** | Effectively yes | Base URL of the signal-cli-rest-api server, for example `http://signal-cli:8080`. |
| **host**, **port**, **use_ssl** | No (legacy) | Superseded by `server_url`; used only when it is empty. |
| **sender_number** | Yes | The number registered with signal-cli, in international format (`+15550001111`). |
| **receiver_id** | Yes | One or more recipients (see below). |
| **auth_username**, **auth_password** | No | HTTP Basic credentials. signal-cli-rest-api has no authentication of its own, so these belong to whatever proxies it. Both halves are needed. |
| **text_mode** | No | `normal` (default) or `styled`. |
| **notify_self** | No | Also raise the notification on the sending account's own devices. |

### Why `server_url` matters

Scryer builds each plugin's HTTP allowlist from its descriptor plus the
configuration values that parse as URLs. This channel declares no static host, so
`server_url` is what puts the server's origin on the allowlist — a bare `host`
never parses as a URL and leaves the allowlist empty, which denies every request.
The legacy `host`/`port`/`use_ssl` settings are still read so existing
configurations keep their values, and the channel warns when it had to fall back
to them. If Scryer refuses the request, the error says exactly this and names the
setting to fix.

### Recipients

`receiver_id` takes a list. Each value is one of:

* a phone number in international format — `+15550002222`;
* a group id as the server reports it — `group.<base64 id>`;
* a Signal username, optionally with the server's `u:` prefix.

signal-cli-rest-api refuses to mix recipient kinds in one request and refuses
more than one group per request, so the plugin splits the recipients into the
fewest requests it will accept: all numbers together, all usernames together, and
one request per group. Each recipient gets its own `target_results` entry, and a
recipient the server names in `errors.recipients[]` on an otherwise successful
send is reported as a failure rather than being ignored.

## The message

Line 1 is Scryer's event heading (`Grabbed: Example Show`), line 2 is the event
summary, and further `Label: value` lines are added from whichever structured
blocks the event carries — episode, quality, release, release group, indexer,
size, download client, destination or deleted path, health check, version
change, manual-interaction reason and link, subtitle languages. An event with no
extra blocks renders as the two lines Sonarr sends.

`text_mode` is always sent, so the server's `DEFAULT_SIGNAL_TEXT_MODE`
environment variable can never reinterpret text this channel composed. In
`styled` mode the heading is sent bold and every interpolated value is
backslash-escaped for Signal's `*`, `` ` ``, `|` and `~` markup; in `normal` mode
nothing is escaped because the server does no parsing. Bodies over Signal's
2000-character limit are truncated with an ellipsis and a warning.

## Failures

Every failure names the setting that caused it, on ordinary sends as well as on
Test:

| Response | Result |
| --- | --- |
| `201`/`2xx` | Delivered; `timestamp` becomes the delivery id. |
| `2xx` with `errors.recipients[]` | Partial delivery; the named recipients fail with the server's `reason`. |
| `400` "plain HTTP request was sent to HTTPS port" | `InvalidConfig` — use an `https://` server URL. |
| `400` "Invalid group id" | `InvalidConfig` — the group recipient. |
| `400` "Invalid account" | `InvalidConfig` — `sender_number` is not registered with signal-cli. |
| `400`, other | `InvalidConfig` naming the server, quoting its `error`. |
| `401`/`403` | `AuthFailed` — the proxy credentials. |
| `404` | `InvalidConfig` — `/v2/send` is not served at that URL. |
| `429` | Delivery failure carrying `Retry-After`, plus a warning with the `challenge_tokens` and the `/v1/accounts/{number}/rate-limit-challenge` endpoint that clears the rate limit once a captcha is solved. |
| `5xx` | Delivery failure. |
| non-JSON body on any other status | `InvalidConfig` — something that is not signal-cli-rest-api answered. |
| egress refused by Scryer | `InvalidConfig` — set `server_url`. |
| server unreachable | `UpstreamUnavailable` on a connection test; a delivery failure on a live send, so a network blink is never reported as a broken setting. |

On a Test the channel also does one `GET /v1/about` first. It never blocks the
send; it only warns when the URL does not answer like signal-cli-rest-api, when
the server does not list the `v2` API, or when it runs in `MODE=normal`, which
starts signal-cli for every message.

## Not implemented

`base64_attachments`, `sticker`, `mentions`, `quote_*`, `edit_timestamp`,
`link_preview` and `view_once` are accepted by `POST /v2/send` but have no
counterpart in a Scryer notification. Posters are carried as URLs by the
contract, so attaching one would mean fetching and re-uploading the bytes.
