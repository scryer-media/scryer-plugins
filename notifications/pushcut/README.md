# Pushcut

Trigger a named [Pushcut](https://www.pushcut.io) smart notification from Scryer.

Scryer posts one JSON body per notification to
`POST https://api.pushcut.io/v1/notifications/{notificationName}`, authenticated with
Pushcut's `API-Key` header. Pushcut is an iOS/iPadOS/watchOS/visionOS app: the
notification itself is defined in the app, and Scryer only fills in its dynamic
content. This plugin never creates, edits or deletes a Pushcut notification, and it
does not use the Automation Server (`/execute`) or subscription endpoints.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **notification_name** | Yes | The name of the notification defined in the Pushcut app. |
| **api_key** | Yes | A Pushcut API key, from **Account → Add API Key** in the app. |
| **time_sensitive** | No | Sends `isTimeSensitive: true` so the notification breaks through Focus. Ignored when **interruption_level** is set to anything but `inherit`. |
| **interruption_level** | No | `inherit` (default, follow **time_sensitive**), `auto`, `passive`, `active`, `timeSensitive`. |
| **include_poster** | No | Sends the title's poster (or its background image) as the notification's `image`. |
| **metadata_links** | No | Which metadata sites become tap targets: `auto` (default), `none`, `imdb`, `tvdb`, `tvmaze`, `trakt`, `tmdb`, `anidb`, `anilist`, `mal`, `kitsu`. Comma, semicolon or newline separated. |
| **devices** | No | Pushcut device names to target, exactly as they appear in the app. Empty sends to every device on the account. |
| **sound** | No | `none`, `vibrateOnly`, `system`, `subtle`, `question`, `jobDone`, `problem`, `loud`, `lasers`, or the name of a sound imported into Pushcut. Empty uses the notification's own sound. |
| **thread_grouping** | No | `none` (default), `title` or `event`. Sets Pushcut's `threadId` so iOS stacks related notifications. |

Every key that existed before is unchanged, and so are the stored values, so existing
channels keep working. Two things changed shape:

* `metadata_links` is now a multi-value field with the site list attached, instead of
  an unvalidated free-text string. The stored value is still the same
  comma-separated text.
* its default is now `auto` rather than empty, so a channel that never chose a site
  still gets one working tap target. `none` turns every action off.

### Interruption level

Pushcut's notification body carries **either** `interruptionLevel` **or**
`isTimeSensitive` — they are separate branches of the request schema — so this plugin
never sends both.

* `inherit` sends `isTimeSensitive` from the **time_sensitive** switch. This is what
  Sonarr does, and it is the default.
* `auto` sends `interruptionLevel` from the event's severity: `timeSensitive` for
  failures (failed downloads, rejected imports, failed subtitle searches),
  `active` for health warnings, `passive` for everything routine. Pick this if you
  want grabs and imports to arrive quietly and only failures to interrupt you.
* `passive` / `active` / `timeSensitive` send that level on every event.

## Message

The title is the event heading Scryer composes (`Grabbed: Example Show`,
`Import complete: Example Show`, `Download failed: …`).

The body is the event summary followed by whatever the event actually carries —
episode, quality, release, release group, indexer, size, download client,
destination or deleted path, health check detail, version numbers, subtitle
languages, media-request status. Fields that are absent are simply not rendered, so
a sparse event produces the same single line it always did.

Pushcut publishes no length limit for `title` or `text`. This plugin still trims the
title at 250 characters and the body at 2000, adding a warning to the delivery
result, so a pathological event cannot become a multi-kilobyte push.

## Actions

Each selected metadata site whose id the title actually carries becomes a Pushcut
action, and the **first** one is also the notification's `defaultAction` — so tapping
the notification opens the title rather than doing nothing. The remaining sites are
buttons on the expanded notification.

The site is resolved against the title's facet, which is what Sonarr's series-only
link generator cannot do: `auto` prefers TVDb → TVMaze → IMDb → TMDb → AniDB for
episodic libraries and TMDb → IMDb → TVDb for everything else, `trakt` searches by
TVDb id for a series and by TMDb (or IMDb) id for a film, and `tmdb` links `/tv/` or
`/movie/` accordingly. Anime ids (AniDB, AniList, MyAnimeList, Kitsu) are offered
whenever the title carries them.

A `Manual interaction required` event carries its own deep link back into Scryer;
that link takes the default-action slot when it is present and absolute.

Dynamic actions are *merged* with the ones the notification defines in the Pushcut
app. A free Pushcut account is limited to one action per notification, so keep
`metadata_links` short (or `auto`) unless the account has Pushcut Pro.

## Poster

`include_poster` sends the poster URL as Pushcut's `image` field. Pushcut fetches
that URL from the device, so it must be reachable over `https` or from the device's
own local network; a relative path is dropped with a warning and a plain-`http` URL
is sent but flagged. No image bytes are ever uploaded by this plugin.

## Test

The **Test** button does more than send a message:

1. `GET /v1/notifications` lists the notifications the account defines. If
   **notification_name** is not one of them, the test fails with the list of names
   that do exist, instead of a bare `404`. A name that differs only by case is
   corrected to the account's own spelling and used.
2. `GET /v1/devices` (the connection probe Pushcut's own API spec nominates) runs
   when **devices** is set, and fails with the account's active device names if one
   of the configured devices does not exist — Pushcut answers `200` for an unknown
   device and simply delivers nothing.
3. The notification is then sent normally.

Both probes are Test-only, and neither can block a working channel: anything other
than a rejected key or a definite "no such name" is reported as a warning and the
send goes ahead.

## Failures

| What happened | What Scryer sees |
| --- | --- |
| `401`/`403`, key rejected | `AuthFailed`, naming **api_key** |
| `404`, no such notification | `InvalidConfig`, naming **notification_name** |
| `400` naming a setting (device, sound, image, …) | `InvalidConfig`, naming that setting |
| `400` for anything else | `Permanent` — the body this plugin built was refused |
| `detailCode: SIGN_IN_WITH_APPLE*` | `AuthFailed` — the Pushcut account must sign in again in the app |
| `429`, rate limited | delivery failure with `Retry-After` |
| `5xx` | delivery failure, retryable |
| Pushcut unreachable | `UpstreamUnavailable` on a connection test; a delivery failure on a live send |

Pushcut's live API returns **401** for an invalid key, not the `403` Sonarr's
integration tests for, so both are treated as a rejected key.

On success the response's `id` is reported as the delivery id. A `202` (Pushcut
accepted the notification for later delivery) is a success with a warning.

## Not exposed

* `delay` and `scheduleTimestamp` — a Scryer notification is about something that has
  already happened, and `delay` additionally requires a Server Extended subscription.
* `imageData` — the notification contract carries an image URL, not bytes.
* `input`, `homekit`, `shortcut`, `runOnServer`, `online`, `urlBackgroundOptions` on
  actions — these drive Pushcut automations, which is the notification's own
  business, not Scryer's.
* `id` (replace an existing notification on the device), `DELETE /v1/submittedNotifications/{id}`,
  `/v1/execute` and the subscription endpoints.

Upstream reference: `https://api.pushcut.io/openapi.yaml` (rendered at
<https://www.pushcut.io/webapi>), read 2026-09-02.
