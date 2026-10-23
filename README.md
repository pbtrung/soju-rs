# soju-rs

A Rust port of [soju], the user-friendly IRC bouncer. soju connects to
upstream IRC servers on behalf of the user to provide extra functionality:
multiple users and networks, detached channels, chat history playback, and
numerous [IRCv3] extensions.

soju-rs aims to behave exactly like upstream soju (v0.11): same
configuration file, same database (an existing `soju.db` can be reused as is,
older schema versions are migrated), same BouncerServ commands and the same
protocol extensions. It works with [gamja] and other soju-aware clients.

## Usage

* [Getting started](doc/getting-started.md)
* [Man page](doc/soju.1.scd) ([sojuctl](doc/sojuctl.1.scd))

```
soju -config /etc/soju/config
sojuctl -config /etc/soju/config user create -username alice -password secret -admin
```

## Building

Requires the latest stable Rust toolchain and a C compiler (SQLite, the TLS
library and the mimalloc allocator are built from source):

    cargo build --release

The `soju` and `sojuctl` binaries are written to `target/release/`. Release
archives for Linux (x86_64 and aarch64) are built in Docker with
`scripts/build-release.sh`: glibc builds on Arch Linux, static musl builds on
Alpine edge, both from freshly pulled and upgraded images.

## Differences from upstream soju

Not supported:

- File uploads (`file-upload`, `/uploads`, `soju.im/FILEHOST`)
- PostgreSQL (`db postgres`), PAM authentication (`auth pam`) and
  `http+pprof://` listeners
- The `fs` message store (`message-store fs`, ZNC-format log files): use
  `db` (the default)
- `sojudb` and the tools from upstream's `contrib/` directory: users are
  managed with `sojuctl` over the admin socket

Behaviour changes:

- `/msg NickServ SET <anything>` no longer overwrites the saved SASL
  credentials with empty ones; only `SET PASSWORD` is saved
- `METADATA <target> SET <key> <invalid value>` leaves the stored value alone
  (upstream resets it while replying with an error)
- TLS listeners negotiate the `irc` ALPN protocol (`http/1.1` for HTTPS),
  where upstream advertises it but drops it from the per-client
  configuration. Clients offering ALPN protocols but none of these are
  rejected during the TLS handshake
- `certfp generate -bits` accepts RSA key sizes of 2048, 3072 (the default),
  4096 and 8192
- Upstream TLS error messages come from rustls rather than Go's crypto/tls
- Received lines only use the text up to the first CR or LF, including CRs
  in the middle of a TCP line and text after a line break in a WebSocket
  frame; lines with invalid UTF-8 are decoded lossily
- Line breaks in sent messages are replaced with spaces; upstream writes
  them as is, which splits the message into several lines
- Text read back from a database or message logs written by upstream (which
  doesn't validate UTF-8) is decoded lossily
- Server times (e.g. CHATHISTORY bounds) may have from one to nine
  fractional second digits, or none; upstream requires exactly three
- HTTP listeners only speak HTTP/1.1 (no HTTP/2)
- Behind a trusted proxy, the client address is the last element of
  `X-Forwarded-For` (the one the proxy added), and must be an IP address.
  Upstream uses the whole header, so a `client, proxy` list put spaces into
  IRC prefixes and broke clients. Forwarded addresses with whitespace are
  ignored
- Upstream servers whose clock is ahead of the bouncer's are handled (from
  the unmerged upstream PR #430): CHATHISTORY LATEST/AFTER/AROUND aren't
  bounded by the bouncer's clock, and MARKREAD may set a read marker up to
  the latest message even when it's in the future
- `soju -version` prints the version, which is also logged on startup
- With `message-expiry` and the `db` message store, the search index is
  optimized after expired messages are deleted, and the SQLite database is
  vacuumed on startup and when a quarter of it or more is unused (upstream
  leaves it to the admin)

## Testing

`cargo test` runs unit tests and the integration tests ported from upstream,
the latter on both the multi-threaded and the current-thread runtime (soju-rs
uses the current-thread one on machines with a single CPU). The port was also
checked with:

- differential tests running the same client sessions against upstream Go
  soju and soju-rs (both connected to [ergo]) and comparing normalized
  transcripts
- end-to-end tests of gamja in headless Chromium against soju-rs
- IRCv3 specification checks (CAP 302, SASL 3.2, labeled-response, batch,
  echo-message, server-time, chathistory, read-marker, metadata-2,
  bouncer-networks)

## License

AGPLv3, see LICENSE.

Copyright (C) 2020 The soju Contributors

[soju]: https://soju.im
[IRCv3]: https://ircv3.net/
[gamja]: https://codeberg.org/emersion/gamja
[ergo]: https://ergo.chat
