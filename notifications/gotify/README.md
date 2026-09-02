# Gotify

Push notifications to a self-hosted [Gotify](https://gotify.net) server.

Scryer posts one message per notification to `POST {server}/message`, authenticated
with the application token in Gotify's documented `X-Gotify-Key` header. The body is
JSON, which is what Gotify requires before it will accept the `extras` object that
carries the markdown flag, the notification image and the click target.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **server** | Yes | Gotify base URL, for example `https://gotify.example`. Must be an absolute `http://` or `https://` URL. |
| **app_token** | Yes | The Gotify **application** token. Gotify 3 shows it only once, when the application is created or its token is rotated. |
| **priority** | No | `Application default`, or Min (0), Low (2), Normal (5, the default) or High (8). |
| **failure_priority** | No | Priority used when the event is a warning or an error. Defaults to `Same as Priority`. |
| **include_series_poster** | No | Attaches the title's poster. Switches the message to markdown. |
| **metadata_links** | No | Metadata sites to link at the end of the message: `imdb`, `tvdb`, `tvmaze`, `trakt`, `tmdb`, `anidb`, `anilist`, `mal`, `kitsu`. Comma, semicolon or newline separated. |
| **preferred_metadata_link** | No | Which of those links tapping the notification opens. `none`, or one of the site keys. Defaults to `tvdb`. |

Every key is unchanged from earlier versions of this plugin, and so are the stored
values, so existing channels keep working. Three field *types* changed so the
settings form can offer the right control and so a typo is caught instead of being
silently ignored: `priority` and `preferred_metadata_link` are now selects, and
`metadata_links` is a multi-value field.

### Priority

Gotify's clients act on the number
([docs](https://gotify.net/docs/priority)):

| Priority | Android | Web UI |
| --- | --- | --- |
| 0 | no notification | shown, no sound |
| 1 – 3 | icon in the notification bar | shown, no sound |
| 4 – 7 | icon and sound | shown with sound |
| 8 – 10 | icon, sound and vibration | shown with sound |

`Application default` sends no `priority` field at all, so Gotify falls back to the
default priority configured on the application itself. Note that a newly created
Gotify application defaults to `0`, which is "no notification".

`failure_priority` applies when Scryer marks the event as a warning or an error —
failed downloads, rejected imports, failed subtitle searches and health issues. It
is off by default: overriding a deliberate `Min (0)` would un-mute exactly the
channel an operator muted.

## Message

The title is the event heading Scryer composes (`Grabbed: Example Show`,
`Import complete: Example Show`, `Download failed: …`). Gotify renders the title as
plain text, so nothing in it is escaped.

The body is the event summary followed by whatever the event actually carries —
episode, quality, release, release group, indexer, size, download client,
destination or deleted path, health check detail, version numbers, subtitle
languages, media-request status. Fields that are absent are simply not rendered, so
a sparse event produces the same single line it always did.

After that come the poster (when enabled) and one link line per selected metadata
site. A site with no id for this title is skipped rather than rendered as a dead
link.

### Markdown

The message is sent as `text/plain` unless the poster or the metadata links are in
play, at which point it becomes `text/markdown` — the same rule Sonarr uses.

When markdown is in use, every piece of interpolated text is backslash-escaped
first. Release names routinely contain `*`, `_`, `[` and `]`, which would otherwise
render as emphasis or a broken link and lose the characters; Gotify's own
documentation also warns that markdown assembled from external text is an injection
surface. Escaping is invisible to the reader: both markdown renderers Gotify ships
(GitHub Flavored Markdown in the web UI, CommonMark in the Android app) render an
escaped character as itself.

Markdown in the web UI needs gotify/server 2.0.5 or newer, and in the Android app
gotify/android 2.0.7 or newer. The connection test reads the unauthenticated
`GET /version` endpoint and warns if the server is older than that while markdown is
in use; it never blocks a delivery.

### Extras

```json
{
  "extras": {
    "client::display": { "contentType": "text/markdown" },
    "client::notification": {
      "bigImageUrl": "https://…/poster.jpg",
      "click": { "url": "https://thetvdb.com/?tab=series&id=12345" }
    }
  }
}
```

`bigImageUrl` is set only when the poster is enabled and the title's poster is an
absolute `http(s)` URL; a relative one is dropped with a warning rather than
attached dead. `bigImageUrl` needs gotify/android 2.3.0 and `click.url` needs
gotify/android 2.0.10; both are ignored by older clients and by the web UI.

The click target is the manual-interaction deep link back into Scryer when the event
carries one, otherwise the preferred metadata link, otherwise nothing. A test
message links the Scryer project so the click target can be confirmed end to end.

## Errors

Configuration problems are reported as configuration problems, naming the setting to
fix, rather than as failed deliveries:

| What happened | Result |
| --- | --- |
| `server` is not an absolute URL, `app_token` is empty, `priority` is not a number in 0–10, an unknown metadata link | invalid configuration, naming the field |
| `preferred_metadata_link` is not one of the selected links | refused during the connection test; on a live send the message is delivered without a click target and a warning is logged |
| HTTP 401 or 403 with Gotify's error JSON | authentication failed, naming `app_token` |
| HTTP 404 | invalid configuration, naming `server` |
| Any failure whose body is not Gotify's error JSON — an authenticating reverse proxy, a captive portal, an unrelated service on that origin | invalid configuration, naming `server` |
| HTTP 400 | permanent failure; the message this plugin built was rejected |
| HTTP 429 | failed delivery, carrying `Retry-After` |
| HTTP 5xx, or the server being unreachable | failed delivery, carrying the status |

A successful send reports the created message id as the delivery id.

## Scope

This is a push-only integration. It creates messages and does nothing else in
Gotify — no applications, no clients, no library refresh.

## Upstream

Verified against gotify/server v3.1.0 (27 August 2026) and the current
documentation on 2 September 2026: [push
messages](https://gotify.net/docs/pushmsg), [message
priority](https://gotify.net/docs/priority), [message
extras](https://gotify.net/docs/msgextras) and the [REST-API
spec](https://github.com/gotify/server/blob/master/docs/spec.json). The
`X-Gotify-Key` header has been accepted since gotify v1.0.0, so no version gate is
needed for it.
