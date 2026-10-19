# YouTube Indexer

Searches YouTube through Google's YouTube Data API v3, using your own API key, and returns HTTP releases for interactive search. A release points at the video's watch page (`https://www.youtube.com/watch?v=<id>`) and nothing else. The Data API never provides a media URL, so you need a download client that accepts provider pages, such as the MeTube plugin, to grab these releases. The indexer never resolves a stream itself.

This plugin is maintained by Scryer. "Official" means it is a Scryer plugin; it does not mean Google endorses it.

## Configure in Scryer

- **API Key** (required): a Google API key with the YouTube Data API v3 enabled in its Google Cloud project. Scryer stores it encrypted and masks it in the UI. The plugin sends it only to `www.googleapis.com`, in the `X-Goog-Api-Key` request header, so it never appears in a URL. It is also kept out of log lines, error messages, release data and the plugin's cache, and it is never passed to a download client.
- **Safe Search**: `moderate` (default), `none` or `strict`.
- **Channel Allow-List**: optional channel ids (`UC` followed by 22 characters), at most 10. When the list is set, every search is limited to those channels, which is the only way to keep results to a show's official uploads.
- **Region** (advanced): optional two-letter country code, for example `US`. Searches are run for that region, and videos blocked there are marked geo-restricted.
- **Relevance Language** (advanced): optional language code, for example `en`. Results in that language rank first.

Calls go straight from your Scryer to Google. No Scryer service sits in between, and no key or quota is shared.

## Recommended setup: a dedicated YouTube library

Keep YouTube in its own library so its results never mix with Usenet or torrent results:

1. Create a library for it (for example "YouTube") in the **Series** facet, with its own root folder and quality profile.
2. In that library's indexer routing, enable `youtube` and disable every other indexer. In every other library, leave `youtube` disabled, which is the default for a newly added indexer.
3. In that library's download-client routing, enable only your HTTP downloader (for example MeTube), with a category or folder that keeps its output separate. Leave that downloader disabled in every other library.
4. Add titles to the library through the normal catalog flow. These are ordinary catalog series and movies that happen to be on YouTube. Channels are not titles.

## Quota

Google's quota documentation, at the time this plugin was written, gave a project 10,000 units a day. `search.list` has its own cap of 100 calls per project per day, and every results page is one call. `videos.list`, `channels.list` and `playlistItems.list` cost one unit each from the general bucket. Quotas vary by project and change over time, so the plugin never estimates what you have left.

What one search costs:

| Search | `search.list` calls | Other calls |
|---|---|---|
| No allow-list, one page | 1 | 1 `videos.list` |
| Allow-list of *n* channels, one page | *n* | 1 `videos.list` per 50 results |
| Allow-list, season or episode lookup, one page | 0 | 1 `channels.list` on the first page, 1 `playlistItems.list` per channel, 1 `videos.list` per 50 results |

- Every results page is enriched with one batched `videos.list` call, never one call per result.
- The plugin makes no calls of its own outside a search: no prefetch, no polling, no RSS.
- The same search within ten minutes is answered from a small cache and spends no quota.
- With an allow-list, a season or episode lookup reads each channel's uploads playlist instead of searching. This spends no `search.list` quota. It reads at most four pages of 50 uploads per channel, then reports that it stopped at its page limit. Uploads are kept only when every word of the title appears in the video title or the channel name.
- Scryer may load further pages until it has as many results as it asked for, but a search reads at most two `search.list` pages per lane (the unfiltered search or each allow-listed channel) and then reports that it stopped at its page limit, so one search costs at most two `search.list` calls per lane plus one `videos.list` call per page. Keep the allow-list short.
- When the daily quota runs out, searches fail with a distinct "quota exhausted" error until Google resets it.

## What results contain

- **Title**: the video title, with only reversible clean-up so Scryer's release parser can find markers such as `S01E02`, `Episode 2`, `Ep. 2` or `#2`. A trailing ` | Channel` suffix is removed; when the remaining title starts with an episode marker, the channel name is moved to the front (`Episode 2 | Show` becomes `Show - Episode 2`). Brackets that contain only promotional words, such as `[Official Video]`, are dropped. The raw title is always kept in `provider_extra.raw_title`.
- **Release fields**: GUID `youtube:<id>`, watch-page link and info URL, publish time, channel, duration, and audio language when the uploader set one.
- **Availability**, in this order:
  1. `removed` when the video was deleted, rejected or failed processing, or is no longer returned by Google.
  2. `geo_restricted` when it is blocked in your configured region.
  3. `live` or `upcoming` for broadcasts.
  4. `auth_required` when it is private.
  5. `available` otherwise.
  6. `unknown` when the details call failed.
  Live and upcoming results are shown but cannot be grabbed.
- **`provider_extra`** fields, which you can use in Rego rules (for example "only channel X" or "reject made-for-kids"): `channel_id`, `channel_title`, `category_id`, `tags`, `licensed_content`, `made_for_kids`, `definition`, `caption`, `privacy_status`, `live_broadcast_content`, `thumbnail_url` and `duration_seconds`.
- `definition` is Google's `hd` or `sd` flag, not a resolution, so the release's resolution is left empty.
- `caption` only says that captions exist, not in which languages. Subtitles are not listed, because `captions.list` costs 50 units.
- Size is unknown and never estimated.

## Errors

Each of these is reported as its own error, and none of the messages contains the key:

- **Invalid key**: invalid, expired or restricted (`keyInvalid`, `forbidden`).
- **API not enabled**: the Data API is not enabled for the project (`accessNotConfigured`).
- **Quota exhausted**: `quotaExceeded`. Scryer retries the search later.
- **Rate limited**: a short-term limit.
- **Not found**: a channel, playlist or video does not exist.
- **Bad parameter**: for example an invalid channel id.

When one channel fails after another has already returned results, the results Scryer already has are kept.

## Not covered

- Automatic search and RSS. The plugin declares interactive search only.
- Searching by external id (TVDB, TMDB, IMDb). Searches are by title only.
- Channels or playlists as titles. A title has to come from the catalog.
- Stream URLs, resolution heights, sizes and subtitle languages. The Data API does not provide them.
- Members-only, age-restricted or private videos beyond marking them `auth_required`.
