# SendGrid

Send plaintext email through SendGrid’s v3 Mail Send API.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **api_key** | Yes | SendGrid API key, used as a Bearer token. |
| **from** | Yes | Sender email address. |
| **recipients** | Yes | Recipient addresses separated by commas, semicolons, or newlines. |

The plugin performs basic address-shape validation: each address needs one non-empty local part and domain and cannot contain whitespace. SendGrid remains responsible for sender verification and recipient acceptance.

## Delivery

The plugin sends one JSON request with the Scryer summary as the subject and the summary message as text/plain content. All recipients share one personalization. It does not support HTML bodies, templates, categories, attachments, or per-recipient substitutions.
