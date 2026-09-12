# Newznab Indexer

A generic Usenet indexer for services that implement the Newznab API. It supports recent, RSS, automatic, and interactive searches for movies, series, and anime using title, external-ID, season, episode, category, and limit inputs.

## Configure in Scryer

**base_url** is required. Configure **api_key** for a service that requires one. **api_path** defaults to /api and is useful for endpoints such as /api/v1/api or /nabapi. **additional_params** appends provider-specific query parameters to every request.

## Behavior and limits

The shared adapter follows Newznab pagination and normalizes categories, languages, grabs, info URLs, and provider attributes before returning releases to Scryer. It reads the full attribute vocabulary Prowlarr, Sonarr, and Radarr exchange: every id spelling (`tvdbid`/`tvdb`, `tmdbid`/`tmdb`, `imdb`/`imdbid`, `rageid`/`rid`, `tvmazeid`, `traktid`, `doubanid`, `anidbid`), `usenetdate`, `size`, `grabs`, `files`, `language`, `subs`, `genre`, `year`/`imdbyear`, `imdbtitle`, `coverurl`/`poster`, `tag`, the scene/nuke flags (`prematch`, `haspretime`, `nuked`), password metadata, and the book/music fields Prowlarr forwards. Plain `<category>` elements carrying numeric ids count alongside `attr name="category"`. Channel-level API limits are read from `limits` and NNTmux's `apilimits` alike. It is protocol-generic: service-specific metadata, quota behavior, and authentication quirks belong in a dedicated provider plugin rather than this adapter.

Before searching, the adapter reads the endpoint's `t=caps` document and honours its `<searching>` block the way Prowlarr and Sonarr do: `tvsearch` and `movie` requests only carry the id parameters (`imdbid`, `tmdbid`, `tvdbid`, `rid`, `tvmazeid`) and the `season`/`ep` hints the mode lists in `supportedParams` (a listed mode without the attribute means `q` only), a mode marked `available="no"` or absent falls back to plain `t=search`, dropped episode context is folded into the text query (`Title S02E05`), and a mode that accepts no `q` skips the text tier. Capabilities are cached per endpoint for seven days; a document without a `<searching>` block, or an unreachable caps endpoint, leaves the adapter on its permissive request shape (retrying a failed fetch after an hour).
