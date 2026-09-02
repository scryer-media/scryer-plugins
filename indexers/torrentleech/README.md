# TorrentLeech Indexer

An RSS adapter for the private tracker [TorrentLeech](https://www.torrentleech.org).
TorrentLeech publishes one personalised RSS feed per account and that feed takes no
search parameter, so this plugin serves the **recent/RSS poll only** — the same shape
Sonarr ships (`SupportsSearch => false`). A request that carries a query, an id, a
season or an episode is answered empty without spending an upstream call.

## Configure in Scryer

| Field | Required | Default | What it is |
|---|---|---|---|
| `base_url` | yes | `http://rss.torrentleech.org` | The RSS host. The feed URL is this value with your RSS key appended as a path segment. |
| `api_key` | yes | — | Your RSS key: the last path segment of the RSS link on your TorrentLeech profile page (a 20-character token), **not** the whole URL. |
| `minimum_seeders` | no | `1` | Host-side release-selection preference. Nothing is filtered inside the plugin. |
| `user_agent` | no | `scryer-torrentleech-indexer/<version>` | Custom `User-Agent`. |
| `cookie` | no | — | Raw `Cookie` header. Only needed if a Cloudflare clearance cookie has to be supplied by hand. |
| `username` / `password` | no | — | HTTP basic auth, for a reverse proxy in front of the feed. |
| `additional_headers` | no | — | Extra headers, one `Header-Name: value` per line. |

The resulting request is `GET {base_url}/{api_key}` — exactly Sonarr's
`{BaseUrl.Trim().TrimEnd('/')}/{ApiKey}`. If you paste the whole RSS link into
`base_url`, the key is not appended a second time.

### Hosts

* `https://rss.torrentleech.org` — the standard feed (Sonarr's default host).
* `https://rss24h.torrentleech.org` — the same feed over a 24-hour window.

These two are the only hosts that serve RSS. The mirror domains
(`torrentleech.cc`, `torrentleech.me`, `tleechreload.org`, `tlgetin.cc`) answer the
`/{RSSKEY}` path with the site's HTML, and Scryer reports that as a content-type
fault naming the two hosts above.

### http:// is upgraded to https://

Sonarr's default is `http://rss.torrentleech.org` and that URL no longer works:
TorrentLeech is HSTS-preloaded and Cloudflare answers plain HTTP with **403**
(the mirrors answer **301**, which Scryer's plugin HTTP does not follow). The
published default is left at Sonarr's value so existing configurations keep
parsing, and the scheme is upgraded to `https` at request time for TorrentLeech's
own domains only. A host you point at yourself is used exactly as you typed it.

## What the feed carries

Each `<item>` gives:

| Feed | Scryer |
|---|---|
| `<title>` | `title` (HTML entities decoded) |
| `<link>` — `…/rss/download/{id}/{RSSKEY}/{name}.torrent` | `download_url`, `link` |
| `<guid>` — `…/torrent/{id}` | `guid` and `info_url` |
| `<comments>` — `…/torrent/{id}#comments` | `comment_url` |
| `<pubDate>` — RFC 2822 | `published_at`, normalised to RFC 3339 UTC |
| `<category>` — e.g. `Episodes HD` | `categories`, `provider_categories`, `provider_extra.category`, and `provider_extra.category_id` when the name maps to one of TorrentLeech's ids |
| `<description>` — `Category: … - Seeders: N - Leechers: M` | `seeders`, `leechers`, `peers` (their sum), and the raw text in `provider_extra.description` |

Every release also carries TorrentLeech's published hit-and-run rule —
`minimum_seed_ratio: 1.0`, `minimum_seed_time_minutes: 14400` (10 days) — so
Scryer's seeding gate does not remove a grabbed torrent before the tracker is
satisfied. The site allows shorter times for upgraded accounts; this pair is the
conservative floor.

**What the feed does not carry:** no info hash, no magnet URI, no size, no
freeleech/download-multiplier flag, no grabs, no languages. The descriptor says so
rather than claiming otherwise, and no `freeleech` or `tags` metadata is invented.
Size is reported as *unknown* (`None`) rather than Sonarr's `0`, so size rules treat
these releases as unmeasured instead of zero-byte.

## Behaviour and limits

* **No post-filtering.** Every release the feed returns is handed to Scryer, which
  does its own matching — the same contract Sonarr has. The plugin only dedupes by
  guid and applies the host's `limit`.
* **One page.** TorrentLeech's RSS endpoint returns a fixed recent window and takes
  no paging parameter, so the descriptor declares `max_pages: 1` and no page size.
* **Pacing.** One request per 5 seconds, which is Prowlarr's measured
  `requestDelay: 4.1` for TorrentLeech rounded up (Sonarr uses its fleet-wide 2 s).
* The plugin never manages a TorrentLeech account and never controls the torrent
  after Scryer hands it to a download client.

## What Scryer reports when something is wrong

| Condition | What Scryer says |
|---|---|
| Empty or malformed `api_key` / `base_url` | `InvalidConfig`, naming the field |
| Feed returns TorrentLeech's `An error has occured! / Your RSS key is invalid.` item (HTTP **200**) | `AuthFailed` naming `api_key` |
| HTTP 404 (a path that is not `/{RSSKEY}`) | `InvalidConfig` naming `base_url` |
| HTTP 3xx | `InvalidConfig` naming `base_url`, with the `Location` |
| HTTP 401/403 with an HTML body (Cloudflare, or plain http) | `InvalidResponse(UnexpectedContentType)` |
| HTTP 401/403 otherwise | `AuthFailed` |
| HTTP 429 | `Deferred(RateLimited)` with `Retry-After`, else a one-hour floor |
| HTTP 5xx, connect failure, timeout | `Deferred(UpstreamFailure)` |
| HTML body where RSS was expected | `InvalidResponse(UnexpectedContentType)` |
| Body not UTF-8, or not well-formed XML | `InvalidResponse(MalformedBody)` |
| No RSS `<channel>` | `InvalidResponse(InvalidRoot)` |
| Body over 8 MiB | `InvalidResponse(TruncatedBody)` |

The invalid-key case is worth calling out: TorrentLeech answers a bad or revoked
RSS key with **HTTP 200 and a valid RSS document** whose single item is an error
notice with an empty `<link>`. Sonarr parses that item, drops it for having no
download URL, and reports "0 releases" — so a revoked key looks like a quiet feed
for ever. Scryer detects the notice and tells you the key was rejected.

## Maintainer notes

* The plugin does **not** use the shared `rss-common` crate. That crate
  post-filters parsed releases against the request (Sonarr never does, and every
  TorrentLeech `<category>` is a site name that a facet filter would reject), it
  cannot report a typed error, it never reads `<comments>`, and it cannot see the
  invalid-key sentinel. Fetch, classify, parse and assemble are all in-plugin.
* `published_at` must leave the plugin as RFC 3339 UTC: the core parses it with
  `DateTime::parse_from_rfc3339` only, so a raw RFC 2822 `pubDate` would be
  silently dropped.
* The RSS key appears in the feed URL **and** in every download URL as a path
  segment. It is stripped from logs, and the guid is taken from `<guid>` (the
  details page) so a persisted, UI-visible identifier never carries it.
* TorrentLeech also has a JSON browse API (`torrents/browse/list/…`) which does
  support search, freeleech filtering and IMDb ids — Prowlarr uses it — but it
  requires a form login with username, password and an optional 2FA token behind
  Cloudflare. That is why this plugin stays RSS-only, like Sonarr.
