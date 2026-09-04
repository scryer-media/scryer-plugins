# FileList Indexer

A torrent indexer for [FileList](https://filelist.io)'s JSON `api.php` endpoint.
FileList is a Romanian invite-only private tracker; you need an account, and the
plugin needs that account's **passkey** (Profile → Security), not its password.

Supported feeds: recent/RSS polls, automatic search and interactive search, for
**series**, **anime** and **movie** subjects.

## Configuration

| Key | Type | Default | Notes |
|---|---|---|---|
| `username` | text | — | FileList account username. Required. |
| `passkey` | password | — | FileList account passkey. Required. |
| `base_url` | connection URL | `https://filelist.io` | Must be an `http(s)` URL. A mirror with a path (`https://mirror/fl`) is honoured on every URL the plugin builds. |
| `categories` | tag list | `23,21,27` | Category IDs searched for series. |
| `anime_categories` | tag list | — | Category IDs searched for anime. Empty means anime searches are skipped. |
| `movie_categories` | tag list | — | Category IDs searched for movies. Empty means movie searches are skipped. |
| `minimum_seeders` | number | `1` | Host-side release-selection preference. The plugin never filters on it. |

The three category fields are pick-lists in the UI and still accept the legacy
comma-separated form, so existing configurations keep working. At least one of
them must contain an ID or the plugin reports an `InvalidConfig` fault instead
of searching.

Options offered for `categories` / `anime_categories` (Sonarr's set): Anime 24,
Animation 15, TV 4K 27, TV HD 21, TV SD 23, Sport 13, RO Dubbed 28. Options for
`movie_categories` (Radarr's set): Anime 24, Animation 15, Movies SD 1, Movies
DVD 2, Movies DVD-RO 3, Movies HD 4, Movies HD-RO 19, Movies 4K 6, Movies
Blu-Ray 20, Movies 4K Blu-Ray 26, Movies 3D 25, RO Dubbed 28, XXX 7.

## Authentication

Every call carries `username` and `passkey` as query parameters, which is what
FileList's API documentation specifies. The same pair is also sent as an HTTP
Basic `Authorization` header, matching how Sonarr authenticates the endpoint, so
the plugin works behind either front end. Download URLs are built from
`base_url` (`/download.php?id=…&passkey=…`) rather than from the API's own
`download_link`, so a mirror stays honoured.

## Request tiers and the API budget

FileList bills a documented **150 API calls per hour per account**, so the
plugin never fans out where a fall-through will do. Requests are organised into
tiers, exactly like Sonarr's `IndexerPageableRequestChain`: a tier runs, and the
next one runs only if the tier before it returned nothing.

| Request | Tiers, in order |
|---|---|
| Recent / RSS | `action=latest-torrents` over every configured category, `limit` capped at 100 |
| Series or movie, with an IMDb id | `type=imdb` |
| Series or movie, text query | `type=name` |
| Series or movie, id **and** text | `type=imdb`, then `type=name` |
| Anime episode with an absolute number | `type=imdb&season=0&episode={absolute}`, `type=imdb&season={s}&episode={e}`, then the same two as `type=name` |
| Anime season | `type=imdb&season={s}`, then `type=name&season={s}` |

Movie searches append the release year to a name query when the host supplies
one and the query does not already contain it (Radarr's behaviour).

Requests are additionally paced at one start every 2 seconds, matching Sonarr's
per-indexer `RateLimit`. A single automatic search therefore costs 1–2 calls,
and an anime episode search at most 4.

Sonarr issues one name request per scene title. Scryer's core dispatches one
`freetext_alias` search per alias instead, so this plugin issues exactly one
name query per call and lets the host own the alias fan-out.

## What the plugin reports

Per release: title, download URL, info URL, GUID (`FileList-{id}`), size,
publish date, seeders, leechers, peers (seeders + leechers), grab count, the
IMDb id normalised to `tt0000000` form, and the provider category string
(`Seriale HD`) on both `categories` and `provider_categories`.

Flags and leech economics:

* `freeleech` → the `freeleech` indexer flag and `download_volume_factor: 0.0`
* `doubleup` → the `doubleupload` flag and `upload_volume_factor: 2.0`
* `internal` → the `internal` flag

`freeleech` is also reported as a boolean and `internal`/`doubleupload` as
`tags`, because those are the keys Scryer's release-rule engine reads.
`files`, `comments`, `times_completed`, `moderated`, `small_description` and
`category` land in the raw provider metadata. The API's own `download_link` is
deliberately not copied there: it embeds the passkey and the plugin already
publishes an equivalent download URL.

The API exposes **no info hash, no magnet URI, no comment page and no seed
requirements**, and the descriptor says so.

Publish dates arrive as `YYYY-MM-DD HH:MM:SS` with no timezone and are emitted
as RFC 3339 UTC (`2019-01-22T22:20:19Z`), which is the only form Scryer's RSS
staleness tracker parses. An unreadable date is dropped rather than guessed.

## Faults

| Condition | Reported as |
|---|---|
| HTTP 401 / 403, or an API `error` mentioning the passkey or username | `AuthFailed` naming `username`/`passkey` |
| HTTP 429, or an API `error` about the rate limit | deferred `RateLimited`, honouring `Retry-After` and otherwise backing off one hour (the budget window) |
| HTTP 400, or an API `error` about parameters | `InvalidConfig` naming `categories` |
| HTTP 3xx | `InvalidConfig` naming `base_url`, quoting the `Location` |
| HTTP 5xx, connect or TLS failure | deferred `UpstreamFailure` |
| A non-JSON body (Cloudflare, an interstitial) | `InvalidResponse(UnexpectedContentType)` |
| Unparseable JSON | `InvalidResponse(MalformedBody)` |
| A JSON root that is not a torrent array | `InvalidResponse(InvalidRoot)` |
| Missing `username`/`passkey`, an unusable `base_url`, or no category IDs at all | `InvalidConfig` naming the field |

Configuration faults are deliberately hard errors, so a typo surfaces to the
operator instead of putting the indexer into a cooldown.
