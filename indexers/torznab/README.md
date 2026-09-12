# Torznab Indexer

A generic torrent indexer for Torznab endpoints, including Jackett-compatible feeds. It supports recent, RSS, automatic, and interactive searches for movies, series, and anime using title, external-ID, season, episode, absolute-episode, category, and limit inputs.

## Configure in Scryer

**base_url** is required. Configure **api_key** for an endpoint that requires one. **api_path** defaults to /api, and **additional_params** adds query parameters for a non-standard endpoint.

## Behavior and limits

The adapter follows Torznab pagination (up to 100 items per page and 30 pages) with a two-second rate-limit hint. It normalizes seeders, peers, leechers, info hashes, magnet URIs, volume factors, private-tracker flags, and seed requirements, plus language, subtitle, protection, and provider metadata when exposed by the endpoint. Every id spelling Prowlarr emits (`tvdbid`/`tvdb`, `tmdbid`/`tmdb`, `imdb`/`imdbid`, `rageid`/`rid`, `tvmazeid`, `traktid`, `doubanid`) lands in the release's external ids, `genre`, `year`, `coverurl`, and `files` are kept as provider metadata, and `tag` values from Prowlarr's flag vocabulary (`internal`, `scene`, `freeleech`, `neutralleech`, `halfleech`, `exclusive`, `doubleupload`) become indexer flags. Scryer evaluates those releases and submits selected ones to a compatible client.
