# Trakt Collection Sync

Keep a Trakt collection aligned with Scryer media events. This plugin sends structured collection updates; it does not produce user-visible Trakt push notifications.

## Authentication

| Setting | Required | Purpose |
| --- | --- | --- |
| **access_token** | Yes | Trakt OAuth access token. |
| **refresh_token** | Yes | Refresh token used to request a current access token before synchronization. |
| **expires** | Yes | Expiry value retained with the imported OAuth credentials. |
| **auth_user** | No | Displayed authenticated user name. |

Scryer uses the plugin’s startOAuth and getOAuthToken actions to complete the supported OAuth flow. During delivery the plugin first tries the refresh-token service and uses the configured access token if renewal is unavailable.

## Sync behavior

Only events with enough title, episode, movie, or provider-ID data produce a Trakt payload; unsupported or incomplete events are a no-op. File and title deletion events use Trakt’s collection-remove endpoint. Other supported lifecycle events use the collection-add endpoint. The plugin is intentionally collection-only: it does not manage watched history, ratings, lists, scrobbling, or comments.
