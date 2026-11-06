# qBittorrent

This is the full-featured qBittorrent Web API adapter. It supports the modern
qBittorrent 5.2+ bearer API key as well as the established Web UI
username/password session flow.

## Connection and routing

**base_url** is required. Set either **api_key**, or both **username** and
**password**; all three can be blank only when the qBittorrent instance
explicitly permits unauthenticated Web UI access. An API key takes precedence
over credentials.

**routing_mode** selects category (default) or tag handling for Scryer's
isolation value. Other than a tag-mode isolation value and the optional
**imported_tag** below, the plugin adds no tags of its own.
**static_tags** are comma-separated tags added to every torrent. Enable
**add_title_tags** (off by default) to also copy the title's own tags onto each
torrent as they are, without a prefix. Request-level directory routing is
supported; **auto_tmm** remains enabled unless Scryer supplies an explicit
directory. **start_paused**,
**force_start**, and **skip_checking** provide defaults for new work.

## Torrent lifecycle

Magnets and torrent URLs, files, and bytes are supported. The adapter can
pause, resume, force-start, remove with or without data, and apply seed ratio
or time limits. It also forwards qBittorrent's sequential, first/last-piece,
content-layout, auto-management, and skip-checking options when Scryer
supplies them.

Enable **tag_after_import** to apply **imported_tag**, defaulting to
`scryer:imported`, after Scryer verifies an import. This marker is
non-destructive; Scryer's seeding profile remains responsible for eventual
pause or removal. Legacy **post_import_action** values remain readable:
`retain` disables tagging, while all other legacy values enable it.
