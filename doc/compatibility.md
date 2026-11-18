# Compatibility with upstream soju and clients

soju-rs targets upstream soju v0.11 configuration and protocol behavior, with
intentional Rust extensions and differences listed below. This is a compatibility
goal, not a claim that every Go/Rust transcript is identical.

## Client support and verification

Clients supporting `soju.im/bouncer-networks` can discover networks after logging
in without a network suffix. Other clients use one connection per network; see
[getting started](getting-started.md#connect-a-client). Multiplex-aware clients
can attach several networks to one physical connection.

The 2026-11-18 verification tested the local pbtrung Gamja and Senpai forks.
Gamja's four browser scenarios pass against both Rust and pinned Go soju;
Senpai's suite and real-bouncer integration pass against Rust. IRCv3 checks
pass 32/32 and transport checks pass 20/20. The twelve selected Go/Rust
transcript comparisons still differ in capabilities, service output and timing.
These results cover the tested revisions and scenarios, not every client release.
See [the final assessment](audit/final-verification.md#client-and-protocol-compatibility)
for pins, coverage and limits, and [the harness guide](../tests/compat/README.md)
to reproduce checks.

## Differences from upstream soju

### Unsupported features

- File uploads (`file-upload`, `/uploads`, `soju.im/FILEHOST`)
- PostgreSQL (`db postgres`), PAM authentication (`auth pam`) and
  `http+pprof://` listeners
- The `fs` message store (`message-store fs`, ZNC-format log files): use
  `db` (the default)
- `sojudb` and the tools from upstream's `contrib/` directory: users are
  managed with `sojuctl` over the admin socket

### Rust extensions

- `soju.im/multiplex`: a client can attach its bouncer networks to its
  bouncer connection instead of opening one connection per network, see
  [multiplexing](multiplex.md). The tested Gamja and Senpai forks use it when available.
- `soju.im/unread`: CHATHISTORY TARGETS replies tell which conversations
  have unread messages or mentions after their read marker, so a client can
  show them without loading any history, see [unread state](unread.md).
  The tested Gamja and Senpai forks use it when available.

### Behavior differences

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
- Timestamps use Jiff's Unix timescale: leap seconds are constrained to second
  59 and dates outside Jiff's timestamp range are rejected. See the
  [time-library migration notes](audit/dependencies.md#timestamp-compatibility).
- Memory-only history has aggregate retention budgets and store-scoped cursors;
  see [resource limits](resource-limits.md).
- HTTP listeners only speak HTTP/1.1 (no HTTP/2)
- Behind a trusted proxy, the client address is the last element of
  `X-Forwarded-For` (the one the proxy added), and must be an IP address.
  Upstream uses the whole header, so a `client, proxy` list put spaces into
  IRC prefixes and broke clients. Forwarded addresses with whitespace are
  ignored
- Upstream servers whose clock is ahead of the bouncer's are handled: CHATHISTORY LATEST/AFTER/AROUND aren't
  bounded by the bouncer's clock, and MARKREAD may set a read marker up to
  the latest message even when it's in the future
- `soju -version` prints the version, which is also logged on startup
- With `message-expiry` and the `db` message store, the search index is
  optimized after expired messages are deleted, and the SQLite database is
  vacuumed on startup, before users start, and when `message-expiry` is
  changed by a reload, if at least 25% of its pages are free (upstream
  leaves it to the admin)
- The message table is clustered by target and time, so a client loading
  recent history reads a few consecutive pages per channel instead of rows
  scattered over the whole file (see [message storage](message-text-index.md#current-layout)).
  Sparse FTS and guarded updates reduce unnecessary work. The current
  database schema is version 35; earlier soju-rs and upstream versions
  cannot open the upgraded database


## Database compatibility

The current SQLite schema is 35. Supported older SQLite databases migrate on
startup, preserving message IDs and delivery cursors. This is a one-way upgrade:
older soju-rs and upstream Go soju cannot open the resulting schema. Restore a
compatible backup to return to an older binary. See
[storage upgrades](message-text-index.md#upgrades-and-compatibility).

[Documentation index](README.md) · [Project README](../README.md)
