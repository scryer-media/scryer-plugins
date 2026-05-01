# Email Notification Plugin

Official Scryer notification plugin for sending plaintext email through SMTP.

## Configuration

- `smtp_host`: SMTP server hostname.
- `smtp_port`: SMTP server port. Leave empty or set `0` to use the default for the selected security mode.
- `security`: `plain`, `starttls`, or `tls`.
- `from_address`: Sender address used in `MAIL FROM` and the message `From` header.
- `to_addresses`: Recipient addresses separated by commas or newlines.
- `username`: Optional SMTP username.
- `password`: Optional SMTP password.
- `subject_prefix`: Optional prefix prepended to notification subjects.
- `reply_to`: Optional `Reply-To` header.
- `hello_name`: Optional EHLO/HELO name. Defaults to `scryer.local`.

## Security Modes

- `plain`: connects without TLS, default port `25`.
- `starttls`: connects in plaintext, requires the server to advertise `STARTTLS`, then upgrades with certificate validation, default port `587`.
- `tls`: connects with TLS from the start with certificate validation, default port `465`.

TLS verification is mandatory. The plugin does not support insecure certificate bypasses.
