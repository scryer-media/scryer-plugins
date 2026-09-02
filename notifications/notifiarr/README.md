# Notifiarr

Send Scryer's lifecycle events to [Notifiarr](https://notifiarr.com), which
relays them to Discord (and the other chat services it fronts).

Notifiarr is not a webhook you own. Its endpoints are *server-side
integrations*: each one parses a fixed schema and renders it with its own
template. This plugin therefore has to choose which integration it is talking
to, and the choice changes both the wire format and what Notifiarr can show.

## Which integration to use

| | **Passthrough** (default) | **Sonarr** |
| --- | --- | --- |
| Endpoint | `POST /api/v1/notification/passthrough/{api_key}` | `POST /api/v1/notification/sonarr` |
| Events | every event Scryer emits | only events that exist in Sonarr's webhook schema |
| Media types | series, anime and movies, each rendered honestly | TV only — a movie is sent as a series |
| Rendering | this plugin builds the card (title, colour, fields, poster, fanart, footer) | Notifiarr's own Sonarr integration builds the card |
| Routing | the `channel_id` configured here | Notifiarr's per-trigger channel picker on notifiarr.com |
| Setup on notifiarr.com | enable **Passthrough** | enable **Sonarr** and assign it a channel |

**Use Passthrough unless you specifically want Notifiarr's Sonarr integration
UI.** It is the only mode that covers Scryer's whole event surface and the only
one that tells the truth about a movie.

### What the Sonarr mode does and does not do

In `integration = sonarr` the plugin builds a genuine Sonarr `WebhookPayload`
from Scryer's notification contract — camelCase members, no null members,
PascalCase `eventType`, camelCase enum values — so Notifiarr's existing Sonarr
integration parses it exactly as it parses Sonarr's own. Known limits, all of
them reported back to Scryer as delivery warnings:

- **Movies and other non-series facets are sent as a series.** Notifiarr's
  Sonarr integration has no movie shape, so the card will call your film a
  series.
- **Scryer-only events are refused, not faked.** Post-processing complete,
  subtitle downloaded, subtitle search failed and the four media-request events
  have no member of Sonarr's `WebhookEventType`. Sending them under a borrowed
  event type would be a guaranteed rejection, so the plugin answers with a typed
  "unsupported" error naming the passthrough integration instead.
- **A failed download is sent as `ManualInteractionRequired`.** Sonarr's webhook
  schema has no failed-download event, and claiming `Download` would tell
  Notifiarr an episode imported successfully. `ManualInteractionRequired` is the
  payload that carries `downloadStatus` and `downloadStatusMessages`, which is
  what a failed download actually is.
- **Ids are best-effort.** Scryer's ids are opaque strings and Sonarr's are
  integers; a non-numeric id is sent as `0`, the value Sonarr itself uses for an
  unsaved record.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **api_key** | Yes | Your Notifiarr API key (36 characters), from notifiarr.com under Profile. Sent as `X-API-Key`, and as a path segment on the passthrough endpoint. |
| **integration** | No | `passthrough` (default) or `sonarr`. See the table above. |
| **channel_id** | Passthrough only | Numeric Discord channel id the passthrough notification lands in. Notifiarr's schema marks it required, so the channel refuses to send without it. |
| **notification_name** | No | The app name Notifiarr groups these notifications under. Defaults to `Scryer`. |
| **instance_name** | No | Distinguishes this Scryer from another one in the same Notifiarr account. Sent as `instanceName` on the Sonarr integration and shown in the passthrough footer. |
| **application_url** | No | Externally reachable URL of this Scryer, sent as `applicationUrl`. Scryer's notification contract carries no application URL, so it is configured here. |
| **ping_user** | No | Numeric Discord user id to mention on every passthrough notification. |
| **ping_role** | No | Numeric Discord role id to mention on every passthrough notification. |

Which events reach this channel is Scryer's setting, not a setting here.

## What the passthrough card looks like

The heading is the title plus whatever episode detail Scryer carries
(`Cinder Line - 2x05 - Ember`, or `Cinder Line - 2019-04-01 - Ember` for a daily
series). The colour follows Sonarr's own event colour table, with Scryer's event
severity as an override Sonarr has no equivalent for — a warning tints the card
orange, an error turns it red, and a warning never turns an already-red card
orange. Poster and fanart become the embed thumbnail and image.

Fields are rendered only when the contract actually carries the data: quality,
release group, size, codecs, audio languages, subtitles, indexer, download
client, custom formats and their score, release title, overview, destination
path, and metadata links (TVDB/Trakt/TVmaze for series, TMDB/Trakt for movies,
IMDb for both, and AniDB/AniList/MyAnimeList/Kitsu whenever an anime id is
present). File-delete, application-update, health and rename events get their
own field sets. An event this plugin has no special case for still renders — it
is never a failure.

Discord's embed limits are enforced before sending (title 256, description 4096,
field name 256, field value 1024, footer 2048, 25 fields, 6000 characters
total). Anything trimmed is reported back to Scryer as a warning rather than
being left for Discord to reject.

## Delivery results

Notifiarr is reached over exactly one origin, `notifiarr.com`, and there is no
configurable server.

| Notifiarr's answer | What Scryer is told |
| --- | --- |
| 2xx with `result: success`, or a 2xx that says nothing about a result | delivered |
| 2xx with `result: error` | **failed** delivery, carrying Notifiarr's own message. This is a real case: the API accepts the request and the integration then refuses it. |
| 400 | failed delivery, carrying Notifiarr's message plus a hint naming the integration to enable on notifiarr.com |
| 401 / 403 | configuration error naming `api_key` |
| 404 | configuration error naming the `integration` setting |
| 429 | failed delivery, with `Retry-After` reported. Free accounts are limited to 500 notifications an hour and 12,000 a day; patron accounts to 1,000 and 24,000; subscriber accounts are unlimited. |
| 502 / 503 / 504 | failed delivery — Notifiarr is unavailable |
| 520–524 | failed delivery — Cloudflare could not reach Notifiarr |
| a non-JSON body on any other status | failed delivery, blamed on whatever fronts Notifiarr (usually Cloudflare) rather than on your API key |

Sonarr treats a 400 as a success so that one misconfigured event does not stop
the others. Scryer does not: a notification that was not delivered is reported
as not delivered.

**Testing the channel** additionally probes Notifiarr's own key-validation
endpoint (`GET /api/v1/user/validate`) before sending, so a rejected API key is
distinguishable from an integration that is simply switched off. Anything that
probe finds is a warning; the test notification itself produces the real result.
