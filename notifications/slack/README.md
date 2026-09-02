# Slack

Post Scryer event notifications to a Slack incoming webhook.

Every notification is one message: a plain-text line (what Slack shows in the
channel list and in the push notification) plus a single coloured attachment
whose content is Block Kit.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **webhook_url** | Yes | The incoming-webhook URL, e.g. `https://hooks.slack.com/services/T0/B0/token`. Must be `http(s)`. |
| **username** | Yes | Display name for the message. Defaults to `Scryer`. Rejected if you blank it. |
| **icon** | No | An emoji name wrapped in colons (`:robot_face:`) sent as `icon_emoji`, or an `http(s)` image URL sent as `icon_url`. |
| **channel** | No | Channel override, e.g. `#media`. Sent verbatim. |

### `username`, `icon` and `channel` only work on some webhooks

Slack's own documentation is explicit: with a webhook created from a **Slack
app** you "cannot override the default channel (chosen by the user who installed
your app), username, or icon" — those values always come from the app
configuration. The three settings are still sent, because a **legacy
custom-integration** webhook (the `my.slack.com/services/new/incoming-webhook/`
kind Sonarr links to) does honour them, and so do the Slack-compatible endpoints
below. If you are on an app webhook and your messages ignore these settings,
that is Slack, not Scryer.

### Slack-compatible endpoints

No host allowlist is applied. The incoming-webhook payload is a de facto format
that Mattermost, Rocket.Chat and Discord's `/slack` compatibility endpoint all
accept, and this channel works against them.

## Message layout

**Test message.** A plain-text line only — `Test message from Scryer posted at
<event time>` — with no attachment, matching Sonarr.

**Everything else.**

* Top-level `text` is the event summary (`Grabbed: …`, `Import complete: …`,
  `Download failed: …`).
* One attachment carrying a `color` and a `fallback`, and Block Kit blocks:
  1. a section with the **heading** in bold and the event's message beneath it.
     The heading is the title name plus the episode detail — `Cinder Line - 2x03
     - Trackside`, or `Cinder Line - 2026-08-30 - Friday` for a dated episode —
     falling back to the bare title. Health events head with the health check's
     own source; application-update and manual-interaction events head with the
     Scryer instance name;
  2. a section of up to ten label/value fields, chosen per event from whatever
     the notification actually carries: episode, quality, release, release group,
     indexer, size, download client, import source and destination, file counts,
     deleted paths, health status, versions;
  3. a context line with metadata links (TVDB/Trakt/TVmaze/TMDB/IMDb for an
     episodic title, TMDB/IMDb for a movie, AniDB/AniList/MyAnimeList/Kitsu when
     those ids are present), then the Scryer instance name and the event time.

### Colours

`warning` for grabs, health issues and manual interaction; `danger` for failed
downloads, rejected imports, deletions and failed subtitle searches; `good` for
imports, upgrades, additions, restored health and application updates; and a
neutral blue for renames, tests and submitted or cancelled media requests. A
notification whose severity is an error is always `danger`; a warning severity
raises anything that is not already `danger`.

Note that Scryer's `Download` event is a **failed** download — a successful
import arrives as `ImportComplete` or `Upgrade` — so it renders red and says so.

## Why an attachment at all

Slack calls secondary message attachments a legacy feature and recommends
layout blocks. Block Kit, however, has no colour: the coloured left border only
exists on an attachment. This channel therefore uses Slack's own documented
bridge — an attachment that carries nothing but `color`, `fallback` and a
`blocks` array. None of the legacy content fields (`title`, `text`, `fields`,
`author_name`, `pretext`, `mrkdwn_in`) is used, so if Slack ever reduces
attachments the channel loses a coloured border and nothing else.

Images are deliberately not sent, and the plugin reports `supports_images:
false`. Slack fetches an image URL from its own servers; Scryer poster URLs
routinely point at an instance Slack cannot reach, which renders nothing at best
and can cost the whole message at worst. Sonarr's Slack notification sends no
image either.

## Limits and escaping

`&`, `<` and `>` are escaped to HTML entities in every text object, because
Slack treats them as control characters. Quotes and apostrophes are left alone,
as Slack instructs.

The message text is trimmed to 4,000 characters, a section body to 3,000, a
field to 2,000, and the field list to ten entries. Anything trimmed is reported
back to Scryer as a delivery warning rather than being left for Slack to reject.

## Failures

Slack's webhook errors are plain-text strings, and this channel reads the string
before the status code so the operator is pointed at the right setting:

| Slack says | Result |
| --- | --- |
| `no_service`, `no_service_id`, `no_active_hooks`, `invalid_token`, `no_team`, `team_disabled` | Invalid configuration, naming **webhook_url** — recreate the webhook |
| `channel_not_found`, `channel_is_archived`, `user_not_found`, `posting_to_general_channel_denied` | Invalid configuration, naming **channel** |
| `action_prohibited` | Authentication failure — a workspace restriction on this posting method |
| `invalid_payload`, `no_text`, `too_many_attachments` | Permanent error — the payload is wrong |
| HTTP 429 | Delivery failure carrying Slack's `Retry-After` seconds |
| HTTP 5xx, or anything else | Delivery failure carrying Slack's own response |

Slack rate-limits incoming webhooks to roughly one message per second per
webhook and tolerates short bursts. Scryer does not currently retry a failed
delivery, so a 429 is reported with its retry window rather than absorbed.

The plugin supports no interactive actions; an action request is answered with
`Unsupported`.
