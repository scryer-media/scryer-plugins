# Generic Webhook

Deliver a Scryer event to one HTTP endpoint. This is deliberately small: it supports a fixed URL, two request methods, and one of two body formats.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **webhook_url** | Yes | Destination URL. |
| **method** | No | POST (default) or PUT. |
| **content_type** | No | application/json (default) or text/plain. |

## Payload and responses

With application/json, the request body is Scryer’s normalized structured webhook JSON. A `title_moved` event carries a `title_move` object with the source and destination library and path and whether the move finished with warnings. With text/plain, it is one line in the form [event_type] summary_title: summary_message. The Content-Type and a Scryer webhook User-Agent are set on each request.

Any 2xx response succeeds; non-2xx responses include the returned body in the delivery error. This plugin intentionally has no custom headers, authentication settings, request signing, query templating, or response parsing. Use a receiving relay if those features are needed.
