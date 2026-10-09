# Wizdom Subtitles

A Hebrew-only catalog subtitle provider for Wizdom at https://wizdom.xyz. It looks up releases by IMDb ID and returns the provider's zip artifacts for movies and episodes.

## Configure in Scryer

**base_url** defaults to https://wizdom.xyz and is required. **tmdb_api_key** is optional: Wizdom is keyed entirely on IMDb IDs, so the plugin uses the IMDb ID Scryer already holds, and only falls back to a TMDB title lookup when a key is supplied and Scryer has no IMDb ID for the item. Without a key the provider simply returns no candidates for items that carry no IMDb ID.

The validation action issues a small releases lookup against a known IMDb ID, so it validates the configured endpoint. Wizdom needs no account or credentials.

## Search and download behavior

This provider serves Hebrew (`heb`) only and returns no candidates when Hebrew is not requested. Episode searches need both a season and an episode number; series lookups use the series IMDb ID and movie lookups use the movie IMDb ID.

Wizdom returns the release list in more than one shape. For movies `subs` is a flat array, and for series it is either an array indexed by season number or an object keyed by the season number as a string, with episodes always keyed by the stringified episode number. The plugin accepts all of these. A releases lookup for an IMDb ID the provider does not carry answers with HTTP 500, which is treated as an empty result rather than a provider fault.

The API exposes no hearing-impaired, forced, or AI/machine-translation flags and no uploader or download counts, so those candidate fields are left unset. Media-file hash lookup is not supported. The uploader's release name is surfaced as release info and as a release match hint.

Selected artifacts are fetched from the provider's file endpoint with the release page as the referer and returned with their filename and content type. Archives are deliberately preserved for Scryer's normal archive handling rather than unpacked inside the plugin. Download bodies are capped at 8 MiB, provider rate-limit waits are capped at ten seconds, and retries are bounded.
