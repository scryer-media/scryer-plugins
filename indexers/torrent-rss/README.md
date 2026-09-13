# Torrent RSS Indexer

A generic adapter for torrent tracker and aggregator RSS feeds. It supports recent and RSS feeds only; it cannot issue interactive or automatic search queries to a tracker.

## Configure in Scryer

**feed_url** is required. **download_preference** chooses which available feed download reference to use when an item provides more than one. For protected feeds, configure optional **username** and **password** for HTTP Basic authentication, **cookie** for a raw Cookie header, **user_agent**, and **additional_headers** as one Name: value pair per line.

## Title rewrite rules

**title_rewrite_rules** (advanced) takes one `<regex> => <replacement>` per line, applied in order to each item's title before Scryer parses it; `$1`, `$2` refer to capture groups. Blank lines and lines starting with `#` are ignored. A rule that does not parse fails the search with a line-numbered message. When a rule changes a title, the original is kept as `original_title` in the result's provider metadata. Use it for a group whose naming Scryer cannot read, for example a donghua fansub group that numbers a long-running series by its TVDB season while the title carries a tag the parser mistakes for metadata:

```
^\[FSP\] Battle Through The Heavens NF - (\d{3})( V2)? \[4K\]( V2)?$ => [FSP] Battle Through The Heavens - S05E$1 [2160p]$2$3
```

## Behavior and limits

The plugin fetches the current feed, parses its torrent entries, filters them against the requested feed criteria, and returns at most 200 results. Its two-second rate-limit hint applies to feed requests. Matching quality depends on the names and metadata present in the feed; this plugin has no tracker-specific search or account-management behavior.
