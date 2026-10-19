# soju-rs porting plan

soju-rs is a Rust port of [soju](https://codeberg.org/emersion/soju), the
IRC bouncer. The goal is behavioural compatibility with upstream soju
(v0.11.x) and full compatibility with the
[gamja](https://codeberg.org/emersion/gamja) web client.

## Scope

In scope (ported from upstream):

- IRC message parsing/formatting, the `xirc` helpers, scfg config parsing
- SQLite database with the exact upstream schema and migrations, so an
  existing Go soju `soju.db` can be opened by soju-rs and vice versa
- Message stores: `db` (with FTS5 search), `fs` (ZNC log format), `memory`,
  with byte-compatible internal message IDs (BARE + base64url)
- Authentication: `internal` (bcrypt), `http`, `oauth2`
- Listeners: `ircs://`, `irc://localhost`, `irc+insecure://`, `unix://`,
  `wss://`, `ws://localhost`, `ws+insecure://`, `ws+unix://`, `https://`,
  `http://localhost`, `http+insecure://`, `http+unix://`, `ident://`,
  `http+prometheus://`, `http+insecure+prometheus://`, `unix+admin://`;
  PROXY protocol v1/v2, `Forwarded`/`X-Forwarded-*` headers
- Upstream connections: TLS (with CertFP pinning and SASL EXTERNAL client
  certs), plain-text, Unix sockets, per-user source IPs, identd
- Downstream connections: CAP negotiation, SASL (PLAIN, OAUTHBEARER,
  EXTERNAL, ANONYMOUS), labeled-response, batches, all IRCv3 extensions
  soju advertises, soju extensions (bouncer-networks, read, search, webpush,
  no-implicit-names, account-required, client-cert, metadata-2 pinned/muted/
  blocked)
- BouncerServ service with every upstream command, `sojuctl`
- Web Push (VAPID + RFC 8291 aes128gcm), Prometheus metrics
- Detached channels, auto-detach timers, delivery receipts and backlog,
  chathistory, read markers, auto-away/pre-away, MONITOR, WHO cache

Out of scope (by request): file uploads (`file-upload`, `/uploads`,
`soju.im/FILEHOST`), `contrib/`, database migration tools, `sojudb`,
PostgreSQL. Also not ported: PAM (needs libpam, unavailable for static musl
builds) and `http+pprof` (Go runtime specific).

## Architecture

Mirrors upstream (see upstream `doc/architecture.md`), mapped onto tokio:

- Each connection has a reader task and a writer task (bounded rate limit
  for upstreams). Writers receive messages over an unbounded channel.
- Each user has one task owning all of the user's state (networks, upstream
  connections, downstream connections). Readers post events to it. Event
  handlers are synchronous and only touch state through `&mut User`, so the
  dispatch logic stays race-free like upstream's per-user goroutine.
  Connections and networks are referenced by ID, never by pointer.
- Rare blocking operations inside a handler (Web Push registration check,
  TLS sanity check when auto-creating a network, cross-user service
  commands) use `block_in_place`, like upstream blocks its user goroutine.
- Per-network runner tasks dial upstream servers with exponential backoff
  and hand the socket to the user task, which performs registration.

## Steps

Each step ends with `cargo fmt`, `cargo clippy`, `cargo test` and a commit.

0. Scaffolding: Cargo workspace, AGPL-3.0 license, Rust commit command, plan.
1. Foundations: IRC message type, numerics, `xirc` (case-mapping maps, caps,
   JOIN/ISUPPORT/MOTD/MONITOR/NAMES/SASL generators, WHOX), scfg parser,
   config loader. Unit tests ported from upstream plus parser edge cases.
2. Storage: SQLite database (schema, migrations, all queries), message ID
   encoding, `memory`/`db`/`fs` message stores, ZNC log format. Tests,
   including cross-checks against IDs and databases produced by Go soju.
3. Plumbing: connection abstraction (TCP, TLS, Unix, WebSocket), PROXY
   protocol, rate limiting, auth backends, identd, logger, metrics,
   backoff, certfp generation, Web Push sender.
4. Bouncer core: server, user event loop, networks, upstream and downstream
   connection logic, BouncerServ, admin socket, `soju` and `sojuctl`
   binaries with listeners and SIGHUP reload.
5. Verification:
   - unit and integration tests (fake upstream, scripted downstreams)
   - differential tests against upstream Go soju built from source, both
     bouncers connected to an ergo IRCv3 server, comparing normalized
     transcripts of identical client sessions
   - live checks against the Go soju at 100.100.0.8:6667 (pre-registration)
   - IRCv3 spec checks (CAP 302, SASL 3.2, labeled-response, batch,
     chathistory, read-marker, echo-message, server-time, bouncer-networks)
   - gamja end-to-end in headless Chromium against soju-rs over WebSocket
6. Release v0.99.0: static/dynamic binaries for x86_64 and aarch64, glibc
   and musl, cross-compiled in Docker without QEMU, published with `gh`.
