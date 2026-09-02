# Mailgun

Email through the [Mailgun Messages API](https://documentation.mailgun.com/docs/mailgun/api-reference/send/mailgun/messages).
The channel sends one `POST /v3/{sender_domain}/messages` per event, authenticated
with HTTP Basic as the fixed user `api`, carrying a plain-text body and an HTML
alternative built from Scryer's notification contract.

## Configuration

| Setting | Type | Required | Purpose |
| --- | --- | --- | --- |
| **api_key** | Password | Yes | A Mailgun private API key, or a domain sending key for the sending domain. |
| **use_eu_endpoint** | Bool | No | Send through `api.eu.mailgun.net` instead of `api.mailgun.net`. Must match the region the domain was created in. |
| **sender_domain** | String | Yes | The Mailgun sending domain, for example `mg.example.com`. |
| **from** | String | Yes | The sender address, optionally with a display name: `Scryer <scryer@mg.example.com>`. |
| **recipients** | Tag | Yes | Recipient addresses. |
| **cc** | Tag | No | Additional visible recipients. |
| **bcc** | Tag | No | Additional hidden recipients. |
| **tags** | Tag | No | Mailgun analytics tags (`o:tag`) applied to every message. |
| **send_html** | Bool | No | Send an HTML part alongside the plain text. Default on. |

All five settings the plugin shipped with keep their keys and their stored
values. `recipients` is now a `Tag` field, matching Sonarr's own settings
(`MailgunSettings.cs:40`); Scryer renders a `Tag` field as comma-separated text,
so an existing comma-, semicolon- or newline-separated list keeps parsing
unchanged.

Configuration problems are reported as configuration problems, naming the
setting — an empty or unusable recipient list, a `from` value that is not an
address, a `sender_domain` carrying a scheme or a path, a tag Mailgun cannot
accept. The plugin does not report them as failed deliveries, which is what the
previous version did with an empty recipient list.

Addresses and tags are checked strictly during a connection test and leniently
on a live send: one malformed entry among several fails the test, but a running
channel drops it, records a warning and delivers to the rest. A list with
nothing usable in it is an error either way.

## What is sent

* **subject** — the event heading Scryer composed ("Grabbed: Example Show",
  "Import complete: Example Show", "Download failed: Example Show"), with control
  characters removed and trimmed to 255 characters. Sonarr sends a fixed
  per-event constant instead.
* **text** — the event summary exactly as before, then the facts the event
  carries, one `Label: value` per line: episode, quality, release and release
  group, indexer, size, download client, destination or source path, health check
  and detail, application versions, subtitle languages, media-request status,
  plus the title and the event name.
* **html** — the same content as a small inline-styled table, with the title's
  poster embedded when the contract carries one. Everything is HTML-escaped.
  Turn it off with `send_html` for plain-text-only mailboxes.
* **to** / **cc** / **bcc** — one form parameter per address, the shape Sonarr
  uses. Addresses are deduplicated case-insensitively, and the total is held to
  Mailgun's 1,000-recipients-per-message limit, visible recipients first.
* **o:tag** — the configured tags, at most three of at most 128 ASCII characters.
* **h:X-Scryer-Event-Type** / **h:X-Scryer-Event-Id** — so the message can be
  filtered and correlated with Scryer's own log.

Truncation and other degraded rendering is reported through the delivery
response's warnings, which Scryer logs; the message is still sent.

`Download` events are failures, not imports: Scryer maps a failed download onto
that event type, so the channel renders the client and its status and never a
destination path.

## Delivery outcomes

| Mailgun says | Scryer sees |
| --- | --- |
| `200` with `{"id","message"}` | Queued. The Mailgun message id is the delivery id, one `queued` target result per recipient. |
| `200` from something that is not the Mailgun API | Delivered, with a warning that the endpoint did not answer like Mailgun. |
| `401` | An authentication failure naming **api_key**, mentioning that a domain sending key belongs to one domain and one region. |
| `403` about a sandbox domain | A configuration error naming **recipients** and Mailgun's Authorized Recipients rule. |
| `403` otherwise | An authentication failure: the key is valid but not permitted to send for **sender_domain**. |
| `404` | A configuration error naming **sender_domain** and **use_eu_endpoint** — a domain created in one region does not exist in the other. |
| `400` naming a parameter | A configuration error naming **from**, **recipients**, **tags** or **sender_domain**, carrying Mailgun's own message. |
| `400` naming nothing | A permanent error: the message this plugin built was wrong. |
| `402` | A delivery failure — an account, billing or plan problem nothing in Scryer's settings can fix. |
| `413` | A permanent error: the message this plugin built was too large. |
| `429` | A delivery failure with the retry delay, from `Retry-After` or from `X-RateLimit-Reset` (which Mailgun documents in Unix milliseconds). |
| `5xx` | A delivery failure with `Retry-After` when Mailgun sends one. |

Sonarr turns a 401 into "Unauthorised - ApiKey is invalid" and every other status
into "Unable to connect to Mailgun. Status code: {0}", and only shows either from
its own connection test (`MailgunProxy.cs:33-42`, `Mailgun.cs:81-100`).

Per-recipient results all carry the outcome of the one API call, with the status
`queued` rather than `delivered`: Mailgun accepts or refuses the whole message at
once, and per-recipient delivery is only reported later through its webhooks and
Events API, which this channel does not consume.

## Connection test

A test sends a real message, as Sonarr's does, and additionally reads
`GET /v4/domains/{sender_domain}` once. Everything that probe finds is a warning,
never a failure:

* a `sandbox….mailgun.org` sending domain, which delivers only to addresses added
  and verified under Authorized Recipients — the most common failure on a new
  Mailgun account;
* a domain whose state is `unverified` or `disabled`;
* a domain that does not exist in the selected region.

A *domain sending key* may call only `/messages`, so the probe answering 401 or
403 is expected and is not reported. The Domains API is `v4`; only sending is
still `v3`.

## Deliberate limits

* No attachments, templates, scheduled delivery (`o:deliverytime`), test mode
  (`o:testmode`) or tracking overrides (`o:tracking*`). A Scryer notification is
  a short transactional message; a test that Mailgun accepts but never delivers
  would make the connection test lie.
* No per-recipient variables (`recipient-variables`): every recipient of a Scryer
  notification gets the same message.
* The region is the operator's choice, not something the plugin probes for. A
  wrong `use_eu_endpoint` shows up as a 404 that names the setting.
