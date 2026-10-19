# MeTube

This plugin drives [MeTube](https://github.com/alexta69/metube), a web front
end over yt-dlp, through its HTTP API. It is an `http`-family download client:
Scryer routes it releases from HTTP indexers (a YouTube video, a broadcaster
catalogue page, a direct media file, an HLS or DASH stream) and MeTube's yt-dlp
resolves and fetches them. It is not a Usenet or torrent client.

## Connection and configuration

Set **base_url** to MeTube's address (default `http://localhost:8081`),
including MeTube's `URL_PREFIX` when one is configured. MeTube has no login of
its own; **username** and **password** are only for a reverse proxy in front
of it that asks for HTTP basic authentication.

**download_dir** is MeTube's `DOWNLOAD_DIR` as MeTube sees it (default
`/downloads`, the container image's default). MeTube reports every finished
file by its sub-folder of that directory and its name within the sub-folder,
and the plugin reports completed paths under the directory. When Scryer
mounts the same directory somewhere else, add a remote path mapping for this
client.

**subfolder_per_download** (default on) places each download in its own
sub-folder of the download directory, named after the output name Scryer
derives for the download. It needs MeTube's `CUSTOM_DIRS` and
`CREATE_CUSTOM_DIRS` left at their defaults (on).

## What is sent to MeTube

Every download is one `add` of the release's page URL as a `video` download
that starts at once. Scryer's quality profile and language settings are mapped
onto MeTube's own catalogue:

- the profile's resolution ceiling becomes `quality` (`2160`, `1440`, `1080`,
  `720`, `480`, `360`, `240`; `best` without a ceiling);
- the first preferred video codec MeTube offers becomes `codec` (`h264`,
  `h265`, `av1`, `vp9`; otherwise `auto`);
- `format` is `mp4` when mp4 is the leading container preference and `any`
  otherwise, since MeTube offers no other container choice;
- the first requested subtitle language is sent with MeTube's default
  subtitle mode when the profile asks for subtitles. MeTube currently uses
  these only for its separate `captions` download type, so a video download
  arrives without subtitles.

**Test connection** reads MeTube's version and its `formats` catalogue, and
fails when MeTube does not offer every `quality`, `codec` and `format` value
listed above for a video download (an older MeTube without the catalogue
fails too).

`playlist_item_limit` is always `1`, so a page that turns out to be a list
cannot expand into many downloads. The plugin never sends
`ytdl_options_overrides`: MeTube rejects it unless
`ALLOW_YTDL_OPTIONS_OVERRIDES` is on, and that setting allows command
execution inside the MeTube container.

Because the add runs without a browser `Origin` header, MeTube's cross-origin
guard lets it through with no `CORS_ALLOWED_ORIGINS` change. A proxy that adds
an `Origin` header of its own would need that origin listed.

## Identity

MeTube keys every item by its URL; it has no job id. After each add the
plugin reads MeTube's history and reports the URL MeTube actually stored as
the item id: normally the submitted page URL, or, when yt-dlp rewrote it to
its own page URL, the one item whose yt-dlp id matches the release's item id.
Adding a URL MeTube already has in its queue is reported as success and refers
to the same item. When an add times out (MeTube runs yt-dlp's metadata
extraction before it answers), the plugin checks MeTube's history for the item
before reporting a failure, so a slow provider does not produce a duplicate
submission.

An add MeTube refuses is reported as a permanent failure with MeTube's
message, except a throttled provider (retried) and a sub-folder MeTube will
not create (a configuration error: turn off **subfolder_per_download** or
enable `CUSTOM_DIRS`).

## Queue, completion and removal

The queue listing covers MeTube's `queue` and `pending` lists, with MeTube's
`preparing`, `downloading` and `postprocessing` states shown as resolving,
downloading and muxing. Failed downloads come from MeTube's history with
MeTube's error message. A finished download is reported with its exact file
path under the download directory, so Scryer never has to scan for it. A
finished download whose reported file is missing or lies outside the download
directory is reported as failed instead, and never offered for import. When
MeTube downloads a URL again, the earlier result for that URL is ignored until
the new attempt ends.

Removing an active download cancels it in MeTube. Removing a finished or
failed one clears it from MeTube's history; the file is deleted only when
Scryer clears a history entry and asks for the data to be removed. The plugin
looks the item up first, so a failed download is cleared from MeTube's history
even when Scryer removes it from its queue view, and clearing a history entry
never cancels a new download of the same URL. Note that MeTube's own
`DELETE_FILE_ON_TRASHCAN=true` deletes the file on every clear regardless of
what the plugin sends; leave it at `ask` (the default) or `false` when Scryer
imports by moving files.

Pause, resume and force-start are not available in MeTube and are reported as
unsupported. There are no categories or labels to apply after import.

## Not covered yet

- Subtitle sidecars. MeTube downloads captions as a separate download type, so
  a video add cannot also write sidecar files; the plugin declares that it
  does not write them.
- Separate audio-track selection. MeTube exposes no audio language choice for
  a video download.
