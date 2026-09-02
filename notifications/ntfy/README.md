# ntfy

Publish Scryer notifications to one or more [ntfy](https://ntfy.sh) topics, on
ntfy.sh or on a self-hosted server.

The plugin uses ntfy's **JSON publishing** endpoint: one `POST` to the server
root per topic, with `topic`, `title`, `message`, `priority` and the optional
`tags`, `click` and `icon` fields in the body. Nothing about the notification
travels in the URL, so there is no URL-length ceiling and no encoding hazard for
non-ASCII titles.

## Configuration

| Setting | Type | Required | Purpose |
| --- | --- | --- | --- |
| **server_url** | URL | No | ntfy server base URL; defaults to `https://ntfy.sh`. A path prefix is kept, for reverse-proxied deployments. |
| **access_token** | Password | No | An ntfy access token (`tk_…`), sent as `Authorization: Bearer`. Takes precedence over the username and password. |
| **username** | Text | No | HTTP Basic username. Configure both a username and a password, or neither. |
| **password** | Password | No | HTTP Basic password. |
| **priority** | Select | No | `1` Min, `2` Low, `3` Default, `4` High, `5` Max. Defaults to `3`. ntfy's names (`min`…`urgent`) are also accepted. |
| **failure_priority** | Select | No | Priority used when the event's severity is a warning or an error. Defaults to *Same as Priority*. |
| **topics** | Tags | **Yes** | ntfy topics to publish to. Letters, numbers, underscores and dashes, up to 64 characters. |
| **tags** | Tags | No | ntfy tags. A tag matching an emoji short code (`warning`, `skull`, `+1`) becomes an emoji on the notification; anything else is listed below it. |
| **click_url** | URL | No | Opened when the notification is tapped. `http`, `https`, `mailto:`, `geo:` and app schemes all work. |
| **include_app_name_in_title** | Bool | No | Prefixes the title with the Scryer application name. Off by default. |
| **include_poster** | Bool | No | Sends the title's poster as ntfy's notification `icon`. ntfy renders only JPEG and PNG. |
| **metadata_links** | Tags | No | Metadata sites to link at the end of the message: `imdb`, `tvdb`, `tvmaze`, `trakt`, `tmdb`, `anidb`, `anilist`, `mal`, `kitsu`. Only sites the title has an id for are rendered. |
| **preferred_metadata_link** | Select | No | Which of those links becomes the tap target. Defaults to *None*, which keeps **click_url** as the only click target. |
| **headers** | Multiline | No | Additional headers, one per line as `Header-Name: value`. |

Existing configurations keep working unchanged: every key above that predates
this version has the same name and the same stored values, and the new fields
all default to the previous behaviour.

## What gets sent

* **Title** — the event's own heading (`Grabbed: Example Show`), optionally
  prefixed with the application name. Truncated to ntfy's 1024-byte title limit.
* **Message** — the event summary followed by the facts the event carries:
  episode, quality, release, release group, indexer, size, download client,
  destination or source path, health check detail, application versions,
  subtitle languages, media-request status. Then the selected metadata links.
  Truncated to ntfy's 4096-byte message limit (past which ntfy would silently
  turn the message into a file attachment).
* **Priority** — the configured priority, or **failure_priority** when the event
  is a warning or an error.
* **Tags** — the configured tags, trimmed to ntfy's 512-byte tag budget.
* **Click** — a `ManualInteractionRequired` event's own deep link if it has one,
  otherwise the preferred metadata link for this title, otherwise
  **click_url**.
* **Icon** — the title's poster, when **include_poster** is on.

The message is always plain text. ntfy can render Markdown (web app since server
v2.7.0, Android since v1.17.8), but this channel has no need for it: links go in
`click` and the poster in `icon`, so a release name containing `*` or `_`
arrives exactly as it was written on every client.

## Results and errors

Each topic is published separately and reported separately: the response carries
one `target_results` entry per topic with its own HTTP status and error, and the
delivery counts as successful only if every topic was accepted. This matters on
ntfy, where an access token can be granted write access on one topic and refused
on another.

Failures are attributed rather than merged:

| What happened | How it is reported |
| --- | --- |
| A setting is missing or invalid (topic, priority, URL, half a credential, a malformed header line) | Configuration error naming the field, before anything is sent |
| `401`/`403` | Authentication error naming `access_token`, or `username`, or "authorization required" when neither is configured |
| `404`, or a non-ntfy response body | Configuration error naming `server_url` |
| ntfy error `40009`/`40010` (topic) or `40007` (priority) | Configuration error naming `topics` or `priority` |
| Other `400`, or `413` | Permanent failure of this message |
| `429` | Delivery failure, carrying `Retry-After` when a proxy supplies one |
| `507`, `5xx`, unreachable server | Delivery failure |

When only some topics fail, the whole send is reported as a delivery failure so
the topics that did work are still visible in `target_results`; when every topic
fails for the same configuration reason, it is reported as that configuration
error instead.

A **connection test** additionally performs an unauthenticated
`GET {server_url}/v1/health` first, so an address that is not an ntfy server is
reported as such instead of being mistaken for a credential problem. Anything
that probe finds is a warning; it never blocks a send.

There is no topic discovery or subscription management in the plugin.
