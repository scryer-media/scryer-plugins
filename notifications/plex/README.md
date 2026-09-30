# Plex Media Server

Refresh matching Plex library sections after Scryer imports, upgrades, renames, moves, or removes media. This is a media-server update plugin, not a Plex push-notification integration.

## Server and authentication

| Setting | Required | Purpose |
| --- | --- | --- |
| **base_url** | Yes | Plex server URL, for example http://plex:32400. |
| **api_key** | No | Plex token supplied by Scryer’s media-server integration. |
| **auth_token** | No | Explicit Plex token. It is used when no API key is supplied. |
| **host**, **port**, **use_ssl**, **url_base** | No | Legacy server fields retained for existing configurations. |

The plugin also exposes OAuth actions for connecting a Plex account and listing accessible servers. Those actions are used by Scryer’s setup flow; they are not a substitute for a reachable configured Plex server.

## Refresh targeting

| Setting | Purpose |
| --- | --- |
| **update_library** | Enables targeted refreshes; defaults to true. |
| **section_ids** | Optional library section IDs. If omitted, the plugin discovers relevant TV sections. |
| **path_mappings** | One path rule per line: SOURCE => DESTINATION. Longest applicable rule is used. |
| **map_from**, **map_to** | Older single-pair mapping fields, retained for compatibility. |

For a qualifying event, the plugin resolves the relevant Plex section and requests a refresh scoped to the mapped path when one is available. It does not issue an unrestricted whole-server refresh. Invalid path-mapping syntax and incomplete server configuration fail before a refresh is attempted.

When a title is moved, the plugin refreshes its new folder and its old folder, since the two can sit in different Plex sections. An old folder that no Plex section holds is skipped.
