# Jellyfin Notification Plugin

This plugin sends targeted Jellyfin library refresh requests when Scryer imports, upgrades, renames, moves, or deletes media.

## Setup

1. Install the `jellyfin` notification plugin in Scryer.
2. Create a notification channel using provider type `jellyfin`.
3. Configure the channel with:
   - `base_url`
   - `api_key`
   - `path_mappings`
   - `export_images_metadata` (optional, default `false`)
4. Subscribe the channel to the lifecycle events you want forwarded to Jellyfin.

## Config

- `base_url`
  - Jellyfin server URL, for example `http://jellyfin:8096`
- `api_key`
  - Jellyfin API key
- `path_mappings`
  - Multiline text
  - In the Scryer UI, each row maps the Scryer-visible local path on the left to the matching Jellyfin-visible path on the right
  - Raw format is still one rule per line: `SOURCE_PREFIX => JELLYFIN_PREFIX`
  - Blank lines and lines starting with `#` are ignored
  - Both sides must be absolute paths
  - Longest matching prefix wins
  - Prefix matching is directory-boundary safe

Example:

```text
/data/Movies => /mnt/media/Movies
/data/TV => /mnt/media/TV
/data/Anime => /mnt/media/Anime
```

## Supported Events

- `download`
- `import_complete`
- `upgrade`
- `rename`
- `title_moved`
- `file_deleted`
- `file_deleted_for_upgrade`
- `test`

## Refresh Behavior

- `test`
  - Calls `GET /System/Info`
- `title_moved`
  - Each moved file is sent as its old path `Deleted` and its new path `Created`, through the requests below
  - A move that relocated no media files sends nothing
- Mapped file updates
  - Calls `POST /Library/Media/Updated`
- Unmapped movie updates with provider IDs
  - Falls back to `POST /Library/Movies/Updated` using `tmdbId` and/or `imdbId`
- Unmapped series or anime updates with provider IDs
  - Falls back to `POST /Library/Series/Updated` using `tvdbId`

If an event contains updates that cannot be mapped and also lacks the needed fallback IDs, the plugin returns an error for that notification.

## Artwork and metadata export (opt-in)

With `export_images_metadata` enabled, `import_complete` and `upgrade` events additionally:

1. Resolve the Jellyfin item by one of Scryer's provider ids
   (`GET /Items?AnyProviderIdEquals=<provider>.<id>`; imdb, then tmdb, then tvdb).
2. Push Scryer's `poster_url` and `background_url` to that item as Jellyfin
   `Primary` and `Backdrop` remote images
   (`POST /Items/{id}/RemoteImages/Download`).
3. Set the title fields Scryer carries on the item
   (`POST /Items/{id}`): overview, tags, release date, runtime, studio and sort
   title. Fields Jellyfin derives from the media file itself (stream details,
   certifications, trailers) are deliberately not touched.

Export is best-effort: if no item matches, or the image host fails, the library
refresh still succeeds. The sandbox has no host filesystem access, so this
pushes data into Jellyfin's own storage instead of writing `poster.jpg` or NFO
files beside the media.

## Notes

- The plugin authenticates with Jellyfin using `Authorization: MediaBrowser Token="..."`
- It sends targeted refreshes only. It does not trigger a full Jellyfin library refresh.
- A test event validates the connection through the System/Info endpoint; it does not alter the Jellyfin library.
