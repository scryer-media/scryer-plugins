# MediathekViewWeb Indexer

Searches [MediathekViewWeb](https://mediathekviewweb.de), the public index of the German-language public broadcasters' media libraries (ARD and its regional stations, ZDF, KiKA, ARTE, 3sat, Phoenix, ORF, SRF and others), and returns HTTP releases for interactive search. Each release carries the broadcaster's own MP4 files, its subtitle sidecar when there is one, and the programme page.

This plugin is maintained by Scryer. "Official" means it is a Scryer plugin; it does not mean MediathekViewWeb or any broadcaster endorses it.

## Configure in Scryer

No account or key is needed.

- **Channel Allow-List**: optional channel names as MediathekViewWeb spells them, for example `ARD`, `ZDF`, `KiKA` or `ARTE.DE`; at most 20. With one channel the query itself is limited to it. With several, results from other channels are dropped after the query, so a page can come back with fewer results than were asked for.
- **Include Audio Description** (default off): also return audio-described versions (`Audiodeskription`, `Hörfassung`, `(AD)`).
- **Include Sign Language** (default off): also return versions with sign-language interpretation (`Gebärdensprache`, `(DGS)`).
- **Minimum Duration (minutes)**: optional. Shorter results, such as clips and trailers, are dropped. Results without a duration are kept.

Calls go straight from your Scryer to `mediathekviewweb.de`. No Scryer service sits in between.

## How searching works

- The search text is matched against each entry's title and topic, newest first. Season and episode numbers are not added to it, because the film list does not number episodes; Scryer's release parser reads whatever markers the broadcaster put in the title.
- A page holds up to 50 results. Scryer may load further pages until it has as many results as it asked for, but one search reads at most two pages and then reports that it stopped at its page limit.
- A search without text makes no call. External ids (TVDB, TMDB, IMDb) are not used.

## What results contain

- **Title**: `<topic> - <title> [<channel>]`. When the title already starts with the topic, the topic is not repeated. The raw title is kept in `provider_extra.raw_title`.
- **Release fields**: GUID `mediathekviewweb:<id>`, the programme page as link and info URL, publish time, channel and duration.
- **Resources**, one release per film-list entry:
  - one `direct_file` resource per video quality (low, default, HD), each `mp4` with the frame height when the file name states it (`_AVC-720`, `-1080`, `_1280x720`, `_720p`). A stream URL (`.m3u8`) becomes an `hls` resource.
  - The API's size is a rounded estimate for the default quality only, so it is reported as `size_estimate` on that resource and never as an exact size.
  - a `subtitle` resource when the entry has a subtitle URL. Its format comes from the URL: `.ttml`/`.xml` and ARD's `ebutt` paths are TTML, `.vtt` is WebVTT, `.srt` is SRT. Other URLs, such as KiKA's, name no format, and none is claimed.
  - a `provider_page` resource for the programme page, for clients that resolve pages themselves (MeTube, for example).
- **Accessibility versions** are separate releases, never extra qualities of the plain one. They are recognised from the title and topic (`Audiodeskription`, `Hörfassung`, `(AD)`, `Gebärdensprache`, `(DGS)`) and, because ARD often leaves the title plain, from the content id in an ARD Mediathek page URL: the last segment of `ardmediathek.de/video/…` is a base64 `crid://` id, and one ending in `/audiodeskription` or `/hoerfassung` marks audio description, `/gebaerdensprache` or `/dgs` sign language. Audio-described versions carry the `audio_description` audio role on their media resources. The SDK has no sign-language role, so sign-language versions are marked only in `provider_extra.sign_language`.
- **Availability** comes from the rights region ARD encodes in its media paths. A `/weltweit/` segment means the file downloads worldwide: availability `available`, `provider_extra.geo` `worldwide`. A `geoavailability_D` segment, or a `/de/` segment on an `ard-mcdn.de` host (`/media/p/public/de/…`, `/medp/ondemand/de/…`), means the CDN refuses requests from outside Germany: availability `geo_restricted`, `provider_extra.geo` `de` and `provider_extra.geo_region` `DE`. A release counts as Germany-only when any of its media files is, and as worldwide only when all of them are. Every other shape, including ZDF's, stays `unknown`, with no `geo` hint. The SDK has no field naming the region, so the region is only in `provider_extra`. The film list does not say whether a file is still online.
- **`provider_extra`** fields, for Rego rules: `raw_title`, `topic`, `channel`, `description` (first 500 characters), `duration_seconds`, `size_estimate`, `audio_description`, `sign_language`, `has_subtitles`, `filmliste_timestamp`, and `geo`/`geo_region` when the media paths name a region.

## Errors

- **Unavailable**: a network failure or an HTTP 5xx answer. Scryer retries later.
- **Rate limited**: an HTTP 429 answer. Scryer defers the search.
- **Refused**: any other HTTP error, an `err` in the answer, or an answer that is not the documented JSON. The API's own message is included.

## Not covered

- Automatic search and RSS. The plugin declares interactive search only.
- Range support and geo blocking are not probed at search time; the region is read from the URL only.
