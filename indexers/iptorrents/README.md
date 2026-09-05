# IPTorrents Indexer

An RSS-only adapter for the private tracker IPTorrents (`iptorrents.com`).

IPTorrents publishes one personalised RSS feed per account and offers **no search
parameter on it**. This plugin therefore serves the recent/RSS poll and nothing
else — the same decision Sonarr made (`IPTorrents.SupportsSearch => false`, and a
request generator that answers every search-criteria overload with an empty
chain). Interactive and automatic searches return no results from IPTorrents,
and no upstream request is made for them.

## Configure in Scryer

| Field | Required | What it is |
|---|---|---|
| `feed_url` | yes | The account's **direct-download** RSS URL |
| `minimum_seeders` | no (default 1) | Host-side release-selection preference |
| `user_agent` | no | Overrides the plugin's own `User-Agent` |
| `cookie` | no | Raw `Cookie` header (e.g. a hand-supplied `cf_clearance`) |
| `username` / `password` | no | HTTP basic auth, for a reverse proxy in front of the feed |
| `additional_headers` | no | Extra headers, one `Header-Name: value` per line |

### The feed URL

Take it from IPTorrents' own RSS page with **Download** ticked. Both shapes the
site has used are accepted:

```
https://iptorrents.com/t.rss?u=USERID;tp=PASSKEY;5;22;65;download        (current)
https://iptorrents.com/torrents/rss?u=USERID;tp=PASSKEY;5;22;65;download (older)
```

The members are separated by `;`, not `&`: `u=` is the user id, `tp=` is the
passkey, the bare numbers are category ids, and the trailing `download` flag is
what makes each item's `<link>` a `.torrent` rather than a details page.

The plugin refuses a URL that is not an RSS endpoint, or that lacks `;download`,
with a **configuration error naming `feed_url`** — not a temporary failure — so a
typo is surfaced to you instead of quietly cooling the indexer down. This
mirrors `IPTorrentsSettingsValidator`, with one deliberate leniency: the markers
are matched case-insensitively, so `T.RSS?…;DOWNLOAD` is accepted.

`feed_url` **is** the credential. It is redacted (`u=REDACTED;tp=REDACTED`) in
every log line, and the release guids the plugin emits have `torrent_pass`
stripped.

### Categories

Which categories the feed carries is decided by the ids in `feed_url`, on the
IPTorrents site — there is no category setting here. The TV ids are
4, 5, 22, 23, 24, 25, 26, 55, 65, 66, 73, 78, 79, 82, 83, 99; **Anime is 60**
(not 4). The plugin ships the full published id/name table and reports both the
site's category name and its numeric id on every release.

## What the feed does and does not carry

Each item has a title, a link, a `pubDate` and a description, and nothing else.

**Reported**

* title (HTML entities decoded, as Sonarr does)
* download URL (the item's `<link>`, or a torrent `<enclosure>` if one appears)
* size, parsed out of the description — both shapes work:
  `Category: TV/x264 Size: 1.37 GB` and `556 MB; TV/x264`
* publish date, converted from RSS's RFC 2822 to **RFC 3339 UTC** (Scryer's
  staleness tracker parses nothing else, so a raw `pubDate` would be dropped)
* the site's category name and id (`provider_categories`, `provider_extra`)
* a stable guid derived from the download URL with the passkey removed
* IPTorrents' hit-and-run minimums — see below

**Not reported, because the feed does not contain them**

* seeders, peers, leechers — the descriptor declares `reports_seeders: false`,
  and Scryer never withholds a release on `minimum_seeders` when the count is
  unknown, so the setting is kept for compatibility but has no effect here
* info hash and magnet URI
* freeleech / volume factors
* a details page or comment page (the `<link>` *is* the download URL)
* any series, film or episode id

### Hit-and-run minimums

Every release is reported with `minimum_seed_ratio = 1.0` and
`minimum_seed_time_minutes = 20160` (336 hours), which is IPTorrents' published
rule: seed to a 1:1 ratio or for 14 days. Prowlarr sets exactly these two values
for the same tracker. Scryer's seeding gate treats them as a **floor** under any
seeding profile that honours tracker minimums, so a release grabbed here is not
removed before the tracker is satisfied. Sonarr instead leaves this to a
per-indexer `SeedCriteria` setting the operator has to fill in by hand.

## Errors you may see

| What happened | What Scryer reports |
|---|---|
| `feed_url` empty, not an http(s) URL, not an RSS endpoint, or missing `;download` | configuration error naming `feed_url` |
| the feed redirected (3xx) | configuration error naming `feed_url`, with the `Location` |
| 401/403 with a plain body | authentication failure |
| 401/403 **or 200** with an HTML page | unexpected content type — the site is likely blocking Scryer (Cloudflare) or the `u=`/`tp=` pair is no longer valid |
| 429 | rate limited, deferred for `Retry-After` (or one hour) |
| 5xx, timeout, connection failure | deferred, upstream failure |
| body is not well-formed XML | malformed body |
| body has no RSS `<channel>` | invalid root |
| body over 8 MiB | truncated body |

An empty `<channel>` is a quiet feed, not an error.

## Notes for maintainers

* The plugin does its own fetch, parse and classification rather than using
  `indexers/rss-common`'s `execute_rss_urls`: that helper turns every failure
  into a `Temporary` error, and it post-filters parsed releases against the
  request's query, episode number and category words — something Sonarr never
  does and Scryer's own decision engine already handles.
* Sonarr fills the missing `<guid>` with `Guid.NewGuid()`, so the same release
  never looks like itself twice. This plugin derives a stable guid from the
  download URL instead.
* IPTorrents is invite-only and fronted by Cloudflare. Adding search would mean
  scraping the `t?q=` HTML page with a session cookie, the way Prowlarr does;
  that needs an HTML parser, cookie-session upkeep and a Cloudflare solver, none
  of which the plugin host provides today.
