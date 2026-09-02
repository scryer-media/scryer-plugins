# HDBits Indexer

A private-tracker indexer for HDBits' JSON API (`POST https://hdbits.org/api/torrents`).
It serves the recent/RSS poll, automatic searches and interactive searches for
series, anime and — when you configure movie categories — films.

HDBits is invite-only. The plugin is only usable with an account, and every call
counts against that account's query budget.

## Configure in Scryer

| Field | Required | Default | Notes |
|---|---|---|---|
| `base_url` | yes | `https://hdbits.org` | Must be an `http(s)` root. A mirror's path is kept (`https://mirror/hdb` → `https://mirror/hdb/api/torrents`). |
| `username` | yes | — | HDBits account username. |
| `api_key` | yes | — | HDBits **passkey** (Profile → Security → Passkey). |
| `categories` | yes | `2,3` | Category IDs searched for series and anime. Sonarr's default is TV + Documentary. |
| `movie_categories` | no | *(empty)* | Category IDs searched for films. **Empty means movie searches issue no upstream call at all.** |
| `codecs` | no | *(empty)* | Restrict every search to these codec IDs. |
| `mediums` | no | *(empty)* | Restrict every search to these medium IDs. |
| `origins` | no | *(empty)* | Restrict every search to these origin IDs. |
| `use_filenames` | no | `true` | Report the torrent's scene filename as the release title. |
| `minimum_seeders` | no | `1` | Host-side release-selection preference; the plugin never filters on it. |

All ID fields accept the pick-list the UI writes and the legacy comma-separated
string (`2,3`), so existing configurations keep working unchanged.

### Published ID tables

| Categories | Codecs | Mediums | Origins |
|---|---|---|---|
| 1 Movie | 1 H.264 | 1 Blu-ray/HD DVD | 0 Undefined |
| 2 TV | 2 MPEG-2 | 3 Encode | 1 Internal |
| 3 Documentary | 3 VC-1 | 4 Capture | |
| 4 Music | 4 XviD | 5 Remux | |
| 5 Sport | 5 HEVC | 6 WEB-DL | |
| 6 Audio Track | | | |
| 7 XXX | | | |
| 8 Misc/Demo | | | |

### `use_filenames`

HDBits publishes two names per torrent: `name` (the uploader's display name,
e.g. `Supernatural S10E17 1080p WEB-DL DD5.1 H.264-ECI`) and `filename` (the
scene release name, e.g.
`Supernatural.S10E17.1080p.WEB-DL.DD5.1.H.264-ECI.torrent`). With
`use_filenames` on — the default, matching Prowlarr — the filename minus its
`.torrent` suffix becomes the release title, which parses more reliably. XXX
content (category 7) and full discs (medium 1) always keep the display name,
because HDBits' filenames for those are not release names. The unused name is
always reported in `provider_extra.name`.

Turn `use_filenames` off to get Sonarr's behaviour (always `name`).

## Search shapes

One search issues **one** upstream call, unless the first tier comes back empty.
Tiers, in order:

| Request | Query body |
|---|---|
| Recent / RSS poll | `{username, passkey, category: series ∪ movie, codec, medium, origin, limit: 100}` |
| Series/anime, TVDB id | `{…, tvdb: {id, season?, episode?}}` |
| Series/anime, TVDB id + air date | `{…, tvdb: {id}, search: "yyyy-MM-dd"}` |
| Daily season (`season` looks like a year) | precise `tvdb: {id, season}` first, then `{tvdb: {id}, search: "{year}-"}` |
| Movie, IMDb id | `{…, category: movie_categories, imdb: {id}}` |
| Free text (no usable id, or the id tier found nothing) | `{…, search: "<sanitised query>"}` |

Notes:

* Every key on the wire is lower-case, which is what Sonarr's camel-case JSON
  serializer produces. The previous version of this plugin sent PascalCase keys
  (`Username`, `Passkey`, `Category`, `Tvdb.Id`, …).
* HDBits accepts `tvdb` only for TV and `imdb` only for film; sending an IMDb id
  with a TV query is refused with status 9 `ImdbTvNotAllowed`. The descriptor
  declares `series`/`anime` → `tvdb_id` and `movie` → `imdb_id`, so the host
  never mixes them.
* Free text is sanitised the way Prowlarr sanitises it: every run of non-word
  characters collapses to a single space.
* One free-text query is issued per call. Scryer's core dispatches a separate
  search per title alias (its `freetext_alias` strategy), so the plugin does not
  loop aliases itself.
* An anime request that carries only an absolute episode number is answered with
  the unscoped series query — HDBits has no absolute numbering. The host's
  season/episode strategy covers the precise form separately.

## Paging and limits

`limit` is capped at 100 and exactly one page is fetched per call, which is what
both Sonarr and Prowlarr do (`PageSize = 100`; Sonarr's HDBits generator yields a
single request per pageable entry). Scryer's host asks for `limit: 1000` on every
search; honouring that literally would mean ten API calls per search against a
budget that answers HTTP 403 when exhausted, so the descriptor declares
`max_pages: 1` and the request is capped instead.

Requests are paced at one every 2 seconds (Sonarr's per-indexer `RateLimit`).

## Errors

HTTP deliveries:

| Delivery | Result |
|---|---|
| 3xx | `InvalidConfig` naming `base_url`, with the `Location` |
| 401 | `AuthFailed` |
| 403 whose body reads like a rate limit | `RateLimited`, deferred; window from `Retry-After`, else from the body ("try again in 15 minutes"), else 900 s |
| 403 otherwise | `AuthFailed` |
| 429 | `RateLimited`, deferred; `Retry-After`, else 3600 s |
| other non-200 / transport failure | `UpstreamUnavailable` + `Deferred(UpstreamFailure)` |
| non-JSON body | `InvalidResponse(UnexpectedContentType)` |
| unparseable JSON | `InvalidResponse(MalformedBody)` |
| `data` is not an array | `InvalidResponse(InvalidRoot)` |
| body over 8 MiB | `InvalidResponse(TruncatedBody)` |

API `status` codes, which arrive inside a 200 body:

| `status` | Meaning | Result |
|---|---|---|
| 0 | Success | — |
| 1 | Failure | `UpstreamUnavailable` + `Deferred(UpstreamFailure)` |
| 2 | SslRequired | `InvalidConfig` naming `base_url` |
| 3 | JsonMalformed | `Permanent` |
| 4 | AuthDataMissing | `InvalidConfig` naming `username`/`api_key` |
| 5 | AuthFailed | `AuthFailed` naming `api_key` |
| 6 | MissingRequiredParameters | `Permanent` |
| 7 | InvalidParameter | `Permanent` |
| 8 | ImdbImportFail | `Permanent` |
| 9 | ImdbTvNotAllowed | `Permanent` |
| other | — | `UpstreamUnavailable` + `Deferred(UpstreamFailure)` |

Configuration faults (missing username/passkey, an unusable `base_url`, no
categories at all) are raised as `InvalidConfig` at search time so a typo is
surfaced rather than cooling the indexer down.

## Result metadata

* `published_at` comes from `utadded` (an exact UTC instant) and falls back to a
  normalised `added`; both leave the plugin as RFC 3339 UTC, which is the only
  form Scryer's staleness tracker parses.
* Volume factors follow HDBits' site-wide leech economics (Prowlarr's reading):
  `freeleech: "yes"` → 0.0; XXX (category 7) → 0.0 down **and** 0.0 up (neutral
  leech); full discs, captures, remuxes, internal releases and all TV and
  Documentary content → 0.5; everything else → 1.0. With the default categories
  (TV + Documentary) that means HDBits releases are normally *half* freeleech,
  which Sonarr never reports.
* Flags and `provider_extra.tags`: `freeleech`, `halfleech`, `neutral_upload`,
  `internal` (`type_origin == 1`) and `exclusive` (`type_exclusive == 1`).
  `provider_extra.freeleech` (boolean) and `provider_extra.tags` are the keys
  Scryer's rule engine and UI actually read.
* `categories` carries the readable category name (`TV`) and
  `provider_categories` the provider's numeric id (`2`).
* `provider_extra` also carries `type_category`/`type_codec`/`type_medium`/
  `type_origin`/`type_exclusive` and their labels, `name`, `filename`,
  `numfiles`, `comments`, `times_completed`, `tvdb_season`/`tvdb_episode` and the
  IMDb block (`imdb_english_title`, `imdb_original_title`, `imdb_year`,
  `imdb_genres`, `imdb_rating`).
* Info hashes are reported lower-cased. `grabs` comes from `times_completed`.
* No magnet URI, no comment page and no per-torrent seed requirement is
  published, and the descriptor says so.

## Known limitations

* Scryer's host never fills `context.air_date`, so the air-date search shape is
  implemented and tested but is only reached by a future host. Daily seasons are
  detected from a season number that looks like a year instead.
* `file_in_torrent`, `snatched_only`, `hash` and `page` are documented request
  members the plugin does not use.
* A "freeleech only" filter is deliberately not implemented: Scryer's rules
  engine owns release filtering, and `minimum_seeders` is likewise host-side.
