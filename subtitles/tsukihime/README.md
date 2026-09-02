# Tsukihime Subtitles

An anime-focused catalog provider that finds subtitle attachments cached alongside releases in Tsukihime's public v1 API. It supports movie and episode requests, recommends the anime facet, and can return forced tracks.

## Configure in Scryer

No credentials are used. **base_url** defaults to https://api.tsukihime.org/v1. **max_results** defaults to 50; **max_detail_fetches** limits per-torrent detail requests; **include_adult** defaults to false.

## Search and download behavior

The plugin resolves available AniDB, AniList, or MyAnimeList identifiers before falling back to title search, then examines matching completed torrent records for cached subtitle tracks. It filters candidates by requested language and supports the languages advertised by Tsukihime, including English, Japanese, Chinese, and common European languages.

Downloads come only from the declared Tsukihime storage origin. Compressed subtitle payloads are capped at 2 MiB. XZ decoding does not happen in this plugin: the compressed attachment crosses Scryer's bounded host archive-extraction boundary, so an archive-extraction plugin that advertises XZ support must be installed, and that extractor's expansion limits and path safety apply. Without one, a download reports an unsupported-capability error rather than falling back to a decoder of its own. The client locally respects Tsukihime's public budgets of 60 API requests and 25 search requests per minute; when exhausted it returns no candidates rather than continuing to query the service.

## Artifact

This plugin ships as a WASI Preview 2 component implementing `scryer:subtitle/subtitle-provider@1.1.0`, whose `process` export is an `async func`. A Scryer whose subtitle host only serves the 1.0.0 world (or is still the Preview 1 command runtime) cannot load it; see `min_scryer_version` in `Cargo.toml`.
