# Wizdom Subtitles

A Hebrew-only catalog subtitle provider for Wizdom at https://wizdom.xyz. It looks up releases by IMDb ID and selects usable subtitles from the provider's ZIP downloads for movies and episodes.

## Configure in Scryer

**base_url** defaults to https://wizdom.xyz and is required. **tmdb_api_key** is optional: Wizdom is keyed entirely on IMDb IDs, so the plugin uses the IMDb ID Scryer already holds, and only falls back to a TMDB title lookup when a key is supplied and Scryer has no IMDb ID for the item. Without a key the provider simply returns no candidates for items that carry no IMDb ID. When the fallback is enabled, it tries distinct title candidates and aliases until subtitles are found.

The validation action issues a small releases lookup against a known IMDb ID, so it validates the configured endpoint. Unlike searches, validation reports an HTTP 500 response as an upstream failure. Wizdom needs no account or credentials.

## Search and download behavior

This provider serves Hebrew (`heb`) only and returns no candidates when Hebrew is not requested. Episode searches need both a season and an episode number; series lookups use the series IMDb ID and movie lookups use the movie IMDb ID.

Wizdom returns the release list in more than one shape. For movies `subs` is a flat array, and for series it is either an array indexed by season number or an object keyed by the season number as a string, with episodes always keyed by the stringified episode number. The plugin accepts all of these. A releases lookup for an IMDb ID the provider does not carry answers with HTTP 500, which is treated as an empty result rather than a provider fault.

The API exposes no hearing-impaired, forced, or AI/machine-translation flags and no uploader or download counts, so those candidate fields are left unset. Media-file hash lookup is not supported. The uploader's release name is surfaced as release info and as a release match hint.

Selected artifacts are fetched from the provider's file endpoint with the release page as the referer. Scryer's host archive service extracts the ZIP with its normal path and expansion limits; an installed ZIP-capable archive extractor is required. The provider checks SRT and MicroDVD SUB cue structure, skips unusable members, and prefers UTF-8 when multiple encoding variants are available. Legacy Hebrew bytes are preserved, line endings are normalized, and the selected subtitle is returned with its filename and format.

Download bodies and selected subtitle members are capped at 8 MiB. HTTP 429 responses return immediately with the provider's numeric `Retry-After` hint, including waits longer than ten seconds, so Scryer can schedule the next attempt. If no numeric hint is present, the provider returns a five-second hint. Transport retries remain bounded.
