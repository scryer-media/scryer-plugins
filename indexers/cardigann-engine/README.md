# Advanced Indexers

The Advanced Indexers plugin (plugin id `cardigann-engine`) runs Prowlarr Cardigann v11 tracker definitions as a Scryer WASI Preview 2 indexer component. It is a generic torrent indexer: supply one definition and its selected tracker origin for each configured indexer.

## Configure in Scryer

Pick a tracker from **definition**. The plugin ships the whole Prowlarr Cardigann v11 corpus, so choosing an entry supplies the definition and prefills **base_url** with that tracker's first declared link.

Choose **Custom** instead to paste a definition into **definition_yaml** — the escape hatch for a tracker that is not bundled yet, and for testing a definition before it lands upstream. `definition_yaml` is required in that mode and ignored in every other one, because a bundled selection is authoritative.

Either way, **base_url** must be one of the loaded definition's `links` or `legacylinks` origins. The plugin rejects an undeclared origin, including one supplied through extra settings.

Use **extra_field_data_json** for definition-specific scalar settings, for example `{"passkey":"…","category":"movies"}`. Its values override identically named normal settings, matching Cardigann definition settings. The standard **username**, **password**, and **cookie** fields are also available. Definitions that declare image CAPTCHA can use **cardigannCaptcha** after the `checkCaptcha` action returns the image payload.

All tracker HTTP requests use Scryer's indexer transport, so the indexer's selected proxy and challenge solver apply to login, search, selector grabs, and CAPTCHA image requests.

## Session and pacing

The tracker session is owned by the component. Every `Set-Cookie` field the tracker sends reaches the guest intact, so the cookie jar, its 30-day expiry, and re-login on session loss are all decided here rather than by the host. The jar persists in component instance state, keyed by definition id and configured origin.

The definition's `requestdelay` (and a `requestDelay` extra setting, whichever is larger) becomes a plugin-owned start-rate gate on the host's monotonic clock. It spaces requests across every concurrent operation in one configured indexer, and defers the search instead of overrunning the operation deadline when the required wait no longer fits.

## Actions

- **`checkCaptcha`** runs the login flow far enough to fetch an image challenge and returns `{"captchaRequest": {"type", "contentType", "imageData"}}` with base64 image bytes.
- **`grab`** takes `{"url": "…"}` (or a bare URL string) and runs the definition's authenticated download flow — `before` requests, download selectors, and torrent validation — returning the resolved URL plus artifact bytes. Scryer's download router uses it so a tracker that will not serve an unauthenticated fetch still yields a torrent.

## Behavior and limits

The engine supports Prowlarr Cardigann v11 HTML, JSON, and XML searches, definition filters, login flows, request delays, tracker category mappings, and download-selector validation. It accepts Scryer's typed query, IDs, categories, limit, season, episode, aliases, and `.Query.Year` from the host's search context when the year is genuinely known.

Music and book search is out of Scryer's scope, so `.Query.Artist`, `.Query.Album`, `.Query.Author`, `.Query.Title`, `.Query.Publisher`, and `.Query.Genre` are simply unbound. A definition that names one behaves as Go's `text/template` does for a missing map key: falsy in a conditional, empty when expanded, never a render error.

## Bundled definitions

`known_cardigann_definitions.v1.jsonl` is the selector index and `known_cardigann_definitions.v1.bodies.jsonl` holds one YAML body per line. Both are generated — regenerate them from a Prowlarr Indexers checkout with:

```
cargo xtask cardigann sync-definitions --source <indexers>/definitions/v11
```

Every candidate has to parse, validate, and reach its first HTTP request under this engine before it can be baked; anything that fails is excluded and named in the command's report. `cargo xtask cardigann sync-definitions --check` fails instead of writing, and the crate's own tests re-run the same gate over everything the asset already contains.

The descriptor deliberately declares no strategy plan: one configured indexer shares a single login session and the definition's request delay, so the host runs search tiers sequentially rather than letting the component fan them out against one cookie jar. It also leaves `search_semantics_version` unset, because a pasted third-party definition cannot attest that a short or empty page means the tracker had nothing more.
