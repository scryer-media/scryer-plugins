# Emby

Send targeted media updates to an Emby server after Scryer imports, upgrades, renames, moves, or removes media.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **base_url** | Yes | Emby server URL, including any reverse-proxy path, for example `http://emby:8096` or `https://media.example.test/emby`. |
| **api_key** | Yes | Emby API key. It is sent only in the `X-Emby-Token` request header. |
| **path_mappings** | No | Up to ten absolute `SOURCE => DESTINATION` mappings, one per line. The longest matching source prefix wins. |

With no path mappings, event paths are assumed to be visible to Emby already. Mappings support Unix, Windows, and UNC paths. If mappings exist but an event path is not mapped, the plugin queries Emby for the corresponding Series or Movie item. Provider IDs take precedence over an exact title-name fallback.

## Emby requests

A channel test sends `GET /System/Info`. Media refreshes are deduplicated by path and update type and sent in one PascalCase `POST /Library/Media/Updated` payload. When item discovery is needed, the plugin sends `GET /Items` with `Recursive=true`, the Series or Movie item type, and `Path,ProviderIds` fields.

Only import-complete, upgrade, rename, title-moved, file-deleted, and file-deleted-for-upgrade events are advertised. A moved title sends each moved file as its old path `Deleted` and its new path `Created`; a move that carried no file paths sends the title's new folder as `Modified`. Update types are `Created`, `Modified`, and `Deleted` exactly.

Failed upstream requests report only the operation and HTTP status. Response bodies, authorization headers, and API keys are not included in plugin errors.
