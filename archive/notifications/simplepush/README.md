# Simplepush

Send a Simplepush notification using its form API.

## Configuration

| Setting | Required | Purpose |
| --- | --- | --- |
| **key** | Yes | Simplepush key for the target application or device. |
| **event** | No | Simplepush event key. |

## Delivery

The Scryer summary title is sent as the Simplepush title and the summary message as the message. If configured, the event key is included unchanged. The plugin does not support priority, images, device discovery, or multiple targets in one channel.
