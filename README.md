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

Requires the latest stable Rust toolchain and a C compiler (SQLite and the
TLS library are built from source):

    cargo build --release

The `soju` and `sojuctl` binaries are written to `target/release/`. Release
archives for Linux (x86_64 and aarch64, glibc and static musl) are built in
Docker with `scripts/build-release.sh`.

## Differences from upstream soju

Not supported:

- File uploads (`file-upload`, `/uploads`, `soju.im/FILEHOST`)
- PostgreSQL (`db postgres`), PAM authentication (`auth pam`) and
  `http+pprof://` listeners
- `sojudb` and the tools from upstream's `contrib/` directory: users are
  managed with `sojuctl` over the admin socket

Behaviour changes:

- `/msg NickServ SET <anything>` no longer overwrites the saved SASL
  credentials with empty ones; only `SET PASSWORD` is saved
- `METADATA <target> SET <key> <invalid value>` leaves the stored value alone
  (upstream resets it while replying with an error)
- TLS listeners negotiate the `irc` ALPN protocol (upstream advertises it but
  drops it from the per-client configuration)
- `certfp generate -bits` accepts RSA key sizes of 2048, 3072 (the default),
  4096 and 8192
- Upstream TLS error messages come from rustls rather than Go's crypto/tls
- Lines received from a WebSocket frame only use the text up to the first
  CR or LF; lines with invalid UTF-8 are decoded lossily

## Testing

`cargo test` runs unit tests and the integration tests ported from upstream.
The port was also checked with:

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
