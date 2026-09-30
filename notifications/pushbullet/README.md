# Pushbullet

Sends Scryer notifications as [Pushbullet](https://www.pushbullet.com/) pushes: to channels you own, to specific devices, or to every device on your account.

## Getting an access token

1. Sign in at <https://www.pushbullet.com/> and open your Account Settings page (<https://www.pushbullet.com/#settings/account>).
2. Create an access token in the **Access Tokens** section and copy it.
3. Paste it into **Access Token** in Scryer's notification settings.

Pushbullet's documentation warns that the token has full access to your account, so treat it like a password.

## Configuration

| Setting | Key | Required | Purpose |
| --- | --- | --- | --- |
| **Access Token** | `api_key` | Yes | Your Pushbullet access token. |
| **Device IDs** | `device_ids` | No | Device idens to push to, one push per device. Separate entries with commas, semicolons or new lines. |
| **Channel Tags** | `channel_tags` | No | Tags of channels your account owns, one push per channel. When set, **Device IDs** is ignored. |
| **Sender ID** | `sender_id` | No | The iden of the device the pushes are sent from (Pushbullet's `source_device_iden`). Leave empty to send from the account. A **Test** warns if it is not one of your active devices. |
| **Include App Name In Title** | `include_app_name_in_title` | No | Prefixes the push title with the application name, as in "Scryer - Grabbed: Example Show". Off by default. |
| **Metadata Link** | `metadata_link` | No | The site a push opens when tapped: `auto` (default) picks the best id the title carries, `none` sends plain notes, or pick one of IMDb, TVDb, TVMaze, Trakt, TMDb, AniDB, AniList, MyAnimeList, Kitsu. |

Settings saved by earlier versions of this plugin (0.1.x, 0.2.0) load unchanged; every key is the same.

### Finding device idens

A device iden is an identifier such as `ujpah72o0sjAoRtnM0jc`, not the device's name. Enter what you think is right and run **Test**: if an entry is not one of your active devices, the test fails and lists every device on the account as `name (iden)`. If you entered a device's name instead of its iden, the error tells you the iden to use.

Earlier versions sent an all-digit entry as a numeric `device_id`. The Pushbullet API does not document that parameter and ignores it, sending the push to every device instead, so every entry is now sent as a `device_iden`. A **Test** reports an all-digit entry that does not match a device.

## Routing

Pushbullet allows exactly one target per push, so:

- With **Channel Tags** set, the plugin sends one push to each channel. **Device IDs** is ignored, and a **Test** warns you if both are set.
- Otherwise, with **Device IDs** set, it sends one push to each device.
- With neither set, it sends a single push with no target. Pushbullet delivers that to every device on the account.

Each push reports its own result. If one device or channel fails, the others are still sent, and Scryer is told which target failed and why. A rejected token, the monthly quota (below), a rate limit or an unreachable Pushbullet affects every push the same way, so the plugin stops at the first one. It reports the targets it did not try as `not_attempted`.

Every push for an event carries a `guid` derived from the event and the target. When Scryer sends an event again after a partial failure, Pushbullet returns the push that already went out instead of delivering it again. Pushbullet describes this as "mostly idempotent", so a rare duplicate is still possible.

A push is a **link** that opens the title's metadata page, or the Scryer page for events that need your attention. With no link available, or with **Metadata Link** set to `none`, it is a plain **note**.

## Errors

| What Pushbullet says | What Scryer is told |
| --- | --- |
| The token is missing or invalid (HTTP 401) | Authentication failed on **Access Token**. |
| A device or channel is unknown, or the channel is not the token account's own (HTTP 400 naming `device_iden` or `channel_tag`) | Invalid configuration on **Device IDs** or **Channel Tags**, naming the entry. |
| A channel push is forbidden (HTTP 403) | Invalid configuration on **Channel Tags**. |
| The sender device is rejected | Invalid configuration on **Sender ID**. |
| Rate limited (HTTP 429) | A failed delivery, with the time until Pushbullet's `X-Ratelimit-Reset` as the retry delay. |
| Server error (HTTP 5xx) | A failed delivery for that target, which may be retried. |
| The monthly push quota is used up | Rate limited, with the message below. |

When Pushbullet's request budget is nearly spent (`X-Ratelimit-Remaining` at 5% or less of `X-Ratelimit-Limit`), a successful push carries a warning. That budget is Pushbullet's short-term request rate limit, not the monthly push quota.

## The free-account monthly quota

Pushbullet limits accounts without a [Pushbullet Pro](https://www.pushbullet.com/pro) subscription to **500 pushes a month** sent through the API. Each device and each channel push counts separately, so an event sent to three devices uses three pushes.

When the quota is used up, Pushbullet refuses every further push with "Pushbullet Pro is required to make this call." (error code `pushbullet_pro_required`). Pushbullet uses the same answer for every Pro-only limit, such as file storage, but this plugin sends only note and link pushes, so for it the answer means the monthly quota. The plugin reports that the push needs Pushbullet Pro and names the free monthly limit. The error is typed as rate limited rather than a generic failure, and the plugin stops sending to the remaining targets. If some pushes for the event went out before the limit was hit, the delivery is reported as partly failed, with each target's result.

Pushbullet does not expose the remaining quota or its reset date through the API, so the plugin cannot warn before the quota runs out or say when it will reset. Pushes work again when the quota resets or the account is upgraded to Pro. To make the quota last longer, route to one channel or device rather than several, or turn off the event types you do not need.

## Status

Pushbullet's service and API still work, but the product has received little development since 2022. Its API documentation was last updated in June 2020.
