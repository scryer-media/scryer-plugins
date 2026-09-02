# Nyaa Indexer

Reads Nyaa's RSS endpoint (`https://nyaa.si/?page=rss`) as a public anime torrent
indexer. It serves the recent/RSS poll and automatic/interactive searches: Nyaa renders
any search as RSS, so a poll and a term search are the same endpoint with and without a
search term. There is no API key and no login.

## Configure in Scryer

| Field | Default | Notes |
|---|---|---|
| `base_url` | `https://nyaa.si` | The site root. A search URL is rejected — put category and filter settings in `additional_params`. |
| `anime_standard_format_search` | `false` | Also search with `SxxExx` and season-pack terms. See "What gets searched". |
| `additional_params` | `&cats=1_0&filter=1` | Query parameters appended to the RSS request, each starting with `&`. |
| `minimum_seeders` | `1` | A host-side release preference. The plugin never withholds a release itself. |
| `user_agent`, `cookie`, `username`, `password`, `additional_headers` | — | Optional transport settings for mirrors behind a proxy, or a hand-supplied CDN clearance cookie. Nyaa's own feed needs none of them. |

### `additional_params`

Nyaa accepts two spellings for every search parameter — the modern `q`/`c`/`f`/`p` and
the legacy nyaa.se `term`/`cats`/`filter`/`page` — because its request handler reads them
through `chain_get(req_args, 'q', 'term')` and friends. Both are live today; the first
one present wins. The default keeps the legacy spelling Sonarr ships.

* **Category** — `cats` or `c`, an id such as `1_0` (all anime), `1_1` (AMV), `1_2`
  (English-translated), `1_3` (non-English-translated), `1_4` (raw). The other trees are
  `2_x` Audio, `3_x` Literature, `4_x` Live Action, `5_x` Pictures, `6_x` Software.
* **Quality filter** — `filter` or `f`: `0` none, `1` no remakes, `2` trusted only,
  `3` completed only.
* Also useful: `&u=<uploader>` to follow one release group, `&s=seeders&o=desc` to sort,
  `&m=1` to receive magnet links instead of `.torrent` URLs.

A value Nyaa does not recognise (a malformed category, a filter outside `0..3`) is
answered with HTTP 400, which the plugin reports as a configuration error naming
`additional_params` rather than as a temporary outage.

When the plugin issues a search term of its own, any `q=`/`term=` member in
`additional_params` is dropped from that request — otherwise Nyaa's `q`-before-`term`
precedence would silently discard the search. On a plain RSS poll they are left alone,
because there they are a deliberate standing filter.

## What gets searched

The plugin issues one request per search-term shape, matching Sonarr's
`NyaaRequestGenerator`:

| Request | Terms |
|---|---|
| RSS poll | the bare feed, no term |
| Anime episode with an absolute number | `{title}+{abs}`, plus `{title}+{abs:00}` when the number is below 10 |
| …with `anime_standard_format_search` | also `{title}+s{ss}e{ee}` |
| Single episode (season + episode) | `{title}+s{ss}e{ee}` — **only** with `anime_standard_format_search` |
| Season | `{title}+s{ss}` — **only** with `anime_standard_format_search` |
| Special episode | `{title}+{episode title}` |
| Free text, title or movie search | `{title}` |
| Daily series (a season that is a year) | nothing |

Without `anime_standard_format_search`, a season search and an episode search that
carries no absolute number make **no request at all** — Nyaa numbers releases absolutely,
so an `SxxExx` term would not match, and Sonarr declines the same searches for the same
reason.

The request is never fanned out over `tagged_aliases`: Scryer's host runs its own alias
and id strategy tiers and calls the plugin once per title, so looping here would multiply
every search by the alias count.

**No post-filtering.** Every release the feed returns is handed to Scryer's decision
engine; the plugin only dedupes by guid and applies the host's result limit. Nyaa titles
are routinely romanised differently from the series title, use the Japanese title, or
carry group tags, and the search term already went to Nyaa's own full-text search.

## What the feed carries

Per item: title, `.torrent` link, `guid` (the `/view/<id>` page, reported as the info
URL), `pubDate`, and the `nyaa:` namespace fields `seeders`, `leechers`, `downloads`,
`infoHash`, `categoryId`, `category`, `size`, `comments`, `trusted`, `remake`.

Mapped to Scryer as: `seeders`; `leechers`; `peers` = leechers + seeders (Sonarr's
`CalculatePeersAsSum`); `info_hash_v1` (lower-cased, 40 hex digits only); `size_bytes`
parsed with binary prefixes (`609.6 MiB` → 639 211 930); `grabs` from `nyaa:downloads`;
`categories`/`provider_categories` from `nyaa:category` and `nyaa:categoryId`;
`indexer_flags` and `provider_extra["tags"]` carrying `trusted` and/or `remake` when the
feed sets them; `provider_extra["comments"]` for the comment count.

`published_at` is converted from the feed's RFC 2822 `pubDate` to RFC 3339 UTC, because
Scryer's core parses RFC 3339 and nothing else — a raw `pubDate` is silently dropped.

Not reported, because Nyaa does not publish them: freeleech / volume factors (it is a
public tracker with no leech economy), seed requirements (no hit-and-run rule), languages,
a comment page URL (`nyaa:comments` is a count, not a link). `magnet_url` is reported only
when the feed actually returns magnet links (`&m=1`); by default the link is a `.torrent`.

One page per request: Nyaa returns 75 items per page and Sonarr never pages this indexer.

## Errors

| Condition | Reported as |
|---|---|
| 3xx | `InvalidConfig` naming `base_url`, with the `Location` (the host does not follow redirects) |
| 400 | `InvalidConfig` naming `additional_params` — Nyaa answers 400 for an unknown category or filter |
| 401/403 with an HTML body | `InvalidResponse(UnexpectedContentType)` — a CDN challenge, not a credential |
| 401/403 otherwise | `AuthFailed` — a proxy or mirror in front of Nyaa rejecting `username`/`password`/`cookie` |
| 429 | `Deferred(RateLimited)` with `Retry-After`, else one hour (Sonarr's floor) |
| other non-200, connect/TLS failure | `Deferred(UpstreamFailure)` |
| 200 with an HTML body | `InvalidResponse(UnexpectedContentType)` |
| body over 8 MiB | `InvalidResponse(TruncatedBody)` |
| non-UTF-8 or not well-formed XML | `InvalidResponse(MalformedBody)` |
| no RSS `<channel>` | `InvalidResponse(InvalidRoot)` — `base_url` is not a Nyaa instance |
| unusable `base_url` / `additional_params` | `InvalidConfig` naming the field |

An empty `<channel>` is a quiet feed, not an error.

## Maintainer notes

* The plugin does **not** use `indexers/rss-common`. That crate's `execute_rss_urls`
  post-filters parsed releases against the request — which Sonarr never does, and which
  dropped every result of a `movie`-faceted Nyaa search because each Nyaa category name
  starts with "Anime" — its `fetch_feed` cannot classify a failure (everything becomes
  `Temporary`), and its parser cannot see the `nyaa:` namespace metadata.
* The parser distinguishes namespaced from bare elements on purpose. Sonarr reads `size`,
  `infoHash`, `seeders` and `leechers` with `FindDecendants` (local name, any namespace,
  which is how it reaches `nyaa:size`) but reads `title`, `link`, `guid`, `pubDate` and
  `comments` with `item.Element(name)` (no-namespace only). That is why `nyaa:comments`,
  a count, is never mistaken for Sonarr's `<comments>` URL.
* Search terms are percent-encoded, then joined with `+`. Sonarr's `PrepareQuery` only
  swaps spaces for `+`, so a title containing `&`, `#` or non-ASCII characters corrupts
  its URL.
* The request rate is one start per 2 seconds (Sonarr's `HttpIndexerBase.RateLimit`), and
  the descriptor declares a single strategy at a time, because every request queues behind
  that gate anyway.
* Sonarr's `AdditionalParameters` validator (`Matches("(&[a-z]+=[a-z0-9_]+)*")`) is
  unanchored and therefore accepts everything. This plugin enforces the *shape* instead —
  each member starts with `&` and carries a `name` or `name=value` — but not Sonarr's
  character class, which would reject documented parameters such as `&u=Erai-raws`.
