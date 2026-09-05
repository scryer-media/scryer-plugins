# Discord

Deliver Scryer event notifications to a Discord incoming webhook. The channel renders a
different embed for each event — the way Sonarr's Discord notification does — rather than one
generic message, and the fields inside the grab, import and manual-interaction embeds are
chosen by the operator.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **webhook_url** | Yes | Discord incoming-webhook URL (`https://discord.com/api/webhooks/<id>/<token>`). The legacy `discordapp.com` host is accepted too. |
| **username** | No | Overrides the display name Discord shows for the webhook. |
| **avatar** | No | Overrides the avatar image Discord shows for the webhook. |
| **author** | No | Embed author label; defaults to the Scryer application name. |
| **author_icon_url** | No | Icon shown beside the embed author. Discord renders PNG, JPEG, GIF and WebP; it does **not** render SVG. Empty by default. |
| **grab_fields** | No | Fields rendered on grab notifications. |
| **import_fields** | No | Fields rendered on import, upgrade, import-complete, title-added and title-deleted notifications. |
| **manual_interaction_fields** | No | Fields rendered when a download needs manual interaction. |

### Field sets

The three field settings are tag lists. They carry Sonarr's option names and Sonarr's
defaults, and the order you list them in is the order they are rendered in.

| Option | Grab | Import | Manual | Rendered from |
| --- | :---: | :---: | :---: | --- |
| `overview` | ● | ● | ● | Episode overview, else the title overview; cut at 300 characters |
| `rating` | ● | ● | ● | **Nothing** — Scryer carries no rating (see below) |
| `genres` | ● | ● | ● | **Nothing** — Scryer carries no genres (see below) |
| `quality` | ● | ● | ● | Release quality, else the media file's quality |
| `codecs` | | ● | | Media file video codec / audio codec + channels |
| `group` | ● | ● | ● | Release group, else the media file's release group |
| `size` | ● | ● | ● | Download size, else the sum of the imported files |
| `languages` | | ● | | Distinct audio languages across the imported files |
| `subtitles` | | ● | | Distinct subtitle languages across the imported files |
| `links` | ● | ● | ● | Metadata links for the title's facet (below) |
| `release` | ● | ● | | Release title, else the scene name, in a code block |
| `download_title` | | | ● | Download client item title, in a code block |
| `poster` | ● | ● | ● | Embed thumbnail from the title poster |
| `fanart` | ● | ● | ● | Embed image from the title background art |
| `indexer` | ● | | | Release indexer, else the source hint |
| `custom_formats` | ● | ● | | Custom-format names from the release scores |
| `custom_format_score` | ● | ● | | Sum of the release custom-format scores |

Defaults match Sonarr: every option for grab and manual interaction; every option except
`custom_formats` and `custom_format_score` for import. Clearing a setting renders no fields at
all; leaving it untouched uses the defaults. An option Scryer does not (yet) know is ignored
rather than failing the notification.

**`rating` and `genres` render nothing.** Scryer's notification contract has no carrier for a
title rating or genre list, so the two options exist only so that a configuration migrated
from Sonarr keeps its shape. They will start producing fields when the contract carries the
data; no configuration change will be needed.

A field is dropped whenever the event does not carry its data, so a sparse event simply
produces a shorter embed.

## What each event looks like

| Event | Embed |
| --- | --- |
| Grab | Heading + "Episode Grabbed", standard colour, `grab_fields` |
| Download | Import or upgrade wording and colour; a **failed** download renders "Download Failed" in red |
| Upgrade | "Episode Upgraded", upgrade colour, `import_fields` |
| Import complete | "Import Complete", green, `import_fields` |
| Rename | Message content "Renamed" plus a title-only embed |
| File deleted / deleted for upgrade | Red, with `Reason` and `File name` fields |
| Title added | "Series Added", green, links (plus poster/fanart) |
| Title deleted | Red, the deletion summary as the description, links |
| Health issue / restored | Health source as the heading, health message as the description; amber, or green once resolved |
| Application update | `Previous Version` and `New Version` fields |
| Manual interaction required | "Manual interaction needed", `manual_interaction_fields` |
| Test | A plain message ("Test message from Scryer …") with no embed |
| Everything else | A generic embed with whatever the event carries — quality, indexer, download client, links |

Event wording follows Sonarr's ("Episode Grabbed", "Series Added") when the title's facet is
episodic, and is neutral otherwise ("Grabbed", "Added"). Colours are Sonarr's table; a Scryer
event severity of `warning` or `error` overrides it, and a warning never repaints an event
that is already red.

Headings are built the way Sonarr builds them — `Series - 2x03x04 - Episode + Titles`, or the
air date for a daily episode — with backticks escaped and a 256-character cap. The embed links
to the title's primary metadata page: TVDB/Trakt/TVmaze/TMDB/IMDb for series and anime,
TMDB/Trakt/IMDb for movies, plus AniDB, AniList, MyAnimeList and Kitsu whenever those ids are
present.

## Limits

Discord's documented limits are enforced before the message is sent, so an over-long
notification is trimmed rather than rejected: embed title 256 characters, description 4096,
field name 256, field value 1024, 25 fields, footer 2048, author name 256, 6000 characters
across the whole message, and 2000 characters of message content. Anything trimmed is reported
back to Scryer as a delivery warning, which is logged. Fields are dropped before the
description when the 6000-character budget is exceeded.

## Errors

The channel posts with `wait=true`, so Discord validates the message before answering instead
of silently accepting and discarding a malformed one; the created message id is reported as
the delivery id.

| Discord answers | Scryer sees |
| --- | --- |
| 204, or 200 with the message | Successful delivery (plus any truncation warnings) |
| 400 | A permanent plugin error carrying Discord's own message and error code — the payload is wrong, retrying will not help |
| 401 / 403 | An invalid-configuration error naming `webhook_url` |
| 404 | An invalid-configuration error naming `webhook_url`: the webhook was deleted and must be recreated |
| 429 | A failed delivery carrying Discord's `retry_after` in whole seconds (from the body, or the `Retry-After` / `X-RateLimit-Reset-After` headers) |
| 5xx, or a refused/failed request | A failed delivery carrying the status and Discord's response |

A `webhook_url` that is not an `http(s)` URL is rejected before any request is made.

## Notes

- The webhook's channel decides where messages land; this plugin does not manage Discord
  channels, roles or mentions, and never pings.
- Which events reach this channel is Scryer's setting, not a plugin setting.
- The event timestamp is only stamped on the embed when Scryer sends one; the plugin does not
  read a clock of its own.
