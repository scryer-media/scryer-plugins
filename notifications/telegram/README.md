# Telegram

Sends Scryer's events to a Telegram chat, group, channel or forum topic through
the Bot API's `sendMessage` method, in HTML parse mode.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **bot_token** | Yes | The token @BotFather issued for the bot. |
| **chat_id** | Yes | Numeric chat/group/channel id, or `@username` for a public channel. The bot must be a member of the chat. |
| **topic_id** | No | Forum-topic (message thread) id. Must be greater than 1, or empty for the General topic. |
| **send_silently** | No | Sets Telegram's `disable_notification` — the message arrives without a sound. |
| **include_app_name_in_title** | No | Prefixes the heading with the Scryer application name (`Scryer - Grabbed: …`). |
| **include_instance_name_in_title** | No | **No effect.** Scryer's notification contract carries no instance name. The key is kept so existing configurations keep parsing. |
| **metadata_links** | No | Metadata sites to link at the end of the message: `imdb`, `tvdb`, `tvmaze`, `trakt`, `tmdb`, `anidb`, `anilist`, `mal`, `kitsu`. |
| **link_preview** | No | Which of the selected links Telegram should expand into a preview card, or `none` (the default). |

Event subscriptions are Scryer's, not this plugin's: which events reach the
channel is configured in Scryer, and the plugin renders whatever it is handed.

## Message shape

Every message is:

1. a bold heading — Scryer's own event heading (`Grabbed: Example Show`,
   `Import complete: Example Show`, `Download failed: Example Show`), optionally
   prefixed with the application name;
2. the event's summary sentence;
3. `Label: value` lines for whatever the event actually carries — episode,
   quality, release, release group, indexer, size, download client, destination
   path, deleted file, health check, version numbers, subtitle languages, media
   request status;
4. one `<a href>` line per selected metadata link that the title has an id for.

Nothing is rendered for a block the notification does not carry, so a sparse
event produces a heading and a sentence.

`NotificationEventType::Download` carries a **failed** download, not an import —
Scryer emits successful imports as `ImportComplete`/`Upgrade` — so it renders the
failure status and never an import path.

## Metadata links and previews

Links are generated from the title's external ids and its facet:

| Selection | Series / anime | Movie |
| --- | --- | --- |
| `imdb` | `imdb.com/title/{imdb}` | same |
| `tvdb` | `thetvdb.com/?tab=series&id={tvdb}` | same, when a TVDb id is present |
| `tvmaze` | `tvmaze.com/shows/{tvmaze}` | — |
| `trakt` | `trakt.tv/search/tvdb/{tvdb}?id_type=show` | `trakt.tv/search/tmdb/{tmdb}?id_type=movie`, falling back to the IMDb search |
| `tmdb` | `themoviedb.org/tv/{tmdb}` | `themoviedb.org/movie/{tmdb}` |
| `anidb` / `anilist` / `mal` / `kitsu` | the matching anime page | the matching anime page |

A selected site with no id for that title renders nothing rather than a dead
link. Links appear in the order they are selected.

`link_preview` must name one of the selected `metadata_links` (or `none`). An
unknown value is rejected as a configuration error; a known site that is not
among the selected links fails the connection test, and on a live send the
preview is disabled with a delivery warning so notifications keep flowing.
TVDb is deliberately not a preview option — thetvdb.com serves no preview data. If the chosen preview site
has no id for a particular title, the preview is disabled for that message rather
than silently falling back to a different link.

## Limits and escaping

Telegram caps `text` at 4096 characters *after entity parsing*, so the message is
measured by its visible text — HTML tags and entities do not count against the
budget. When a message would exceed the cap it is trimmed from the end (detail
lines and links go first, the heading and summary stay), the cut is marked with
an ellipsis, and the delivery reports a warning naming what was dropped. `&`,
`<`, `>` and `"` are escaped in text and inside `href` attributes.

## Errors

Problems with the channel's own settings are reported as typed errors naming the
field that has to change, on every send and not only on a test:

| Telegram answer | Reported as |
| --- | --- |
| `401 Unauthorized`, `404 Not Found` | `AuthFailed` — `bot_token` |
| `400 chat not found`, `400 group chat was upgraded to a supergroup chat` | `InvalidConfig` — `chat_id` (the new supergroup id from `parameters.migrate_to_chat_id` is included in the message) |
| `403 Forbidden` (blocked, kicked, not a member) | `InvalidConfig` — `chat_id` |
| `400 message thread not found` | `InvalidConfig` — `topic_id` |
| `400 can't parse entities` / `message is too long` | `Permanent` — a bug in this plugin, not an operator setting |
| any other `400` | `InvalidConfig` — check `bot_token` and `chat_id` |
| `429 Too Many Requests` | delivery failure carrying `retry_after_seconds` from `parameters.retry_after` |
| `5xx`, unreachable host | delivery failure with the provider status |

An invalid `topic_id`, an unknown `metadata_links` value and an unknown
`link_preview` value are all rejected before any request is made. A `link_preview`
that names a site not in `metadata_links` is rejected by the connection test only;
a live send disables the preview and reports a warning instead.

On success the Telegram `message_id` is reported as the delivery id.

## Upstream

Written against Bot API 10.3 (24 August 2026). `link_preview_options` replaced
the removed `disable_web_page_preview` in Bot API 7.0; this plugin has always
used the current field. Only `api.telegram.org` is an allowed egress host, so a
self-hosted local Bot API server is not usable with this plugin today.
