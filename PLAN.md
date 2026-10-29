# soju-c: porting soju-rs to C — implementation plan

Status: **phases 0–9 implemented (9b, Turso, pending); phase 10 next**
(`doc/spikes.md`, `tests/PORTED_TESTS.md`). Phase 4 includes welcome and
state synchronization, dynamic capabilities, multiplex connection views,
and the stateful downstream session fuzzer. The user actor integrates network selection,
channel replay, history stores, receipts and backlog delivery.
Other open items: phase 1's long fuzzing budget and dedicated L5 harnesses;
phase 2's migration check uses upstream's version 0 and 30 schemas rather
than a Go soju v0.11 database. The full phase-exit matrix and long fuzzing
budgets remain separate from the per-commit checks below.
Source of truth for behaviour: soju-rs `main` at v0.99.23 (`c350574`), ~33k
lines of Rust in 90 files (plan first written against v0.99.15, `02acf7d`;
section 8 lists what changed on `main` since then). This branch (`c-port`) starts empty; the Rust tree
stays on `main` and is used as a reference and as a test oracle.

The plan puts the **memory-safety test strategy first** (section 2). It is
designed before any code because it decides how the code is written: who owns
what, how objects die, and which build modes every line must pass.

---

## 1. Goals and non-goals

Goals

- Same observable behaviour as soju-rs: config file syntax and directives,
  SQLite database (read/write the same `soju.db`, run the same migrations),
  IRC wire behaviour, BouncerServ commands, `sojuctl` admin protocol,
  listeners (`ircs`, `irc+insecure`, `unix`, `unix+admin`, `wss`/`https`,
  `ws+insecure`/`http+insecure`, `ws+unix`/`http+unix`, `ident`,
  `http+prometheus`), PROXY protocol, identd, Web Push, OAuth2/HTTP auth,
  SIGHUP reload, message expiry, inactive-user disabling.
- The soju-rs additions: `message-store turso` (Turso Cloud controller
  database plus one database per user, over Turso's SQL-over-HTTP (Hrana)
  protocol and Platform API, including `sojuctl turso create-admin`) and the
  `soju.im/multiplex` extension (`BOUNCER ATTACH`/`DETACH`, virtual
  downstream connections tagged `soju.im/network`).
- Every "Differences from upstream soju" item in the Rust README holds for the
  C port too (lossy UTF-8, CR/LF handling, ALPN, X-Forwarded-For rule,
  clock-skew handling, vacuum policy, ...).
- Zero known memory errors and zero leaks at shutdown, enforced by CI
  (section 2).

Non-goals (same as soju-rs): file uploads, PostgreSQL, PAM, pprof, `fs`
message store, `sojudb`.

---

## 2. Memory-safety strategy and test plan (designed first)

### 2.1 Coding rules that make bugs detectable

These are rules, not suggestions: code review rejects violations, and
`tools/lint.sh` (clang-tidy + grep checks) enforces what it can.

1. **Every object is owned by one loop thread; the number of loops follows
   the CPU count, like soju-rs's Tokio runtime flavor.** soju-rs runs a
   `current_thread` runtime on a single CPU and a `multi_thread` one
   otherwise (`src/bin/soju.rs`). The C port runs N libuv loops, one per
   thread, with N = the number of CPUs (`uv_available_parallelism()`):
   - **N = 1**: one loop thread owns everything; events between objects are
     plain function calls and same-loop queues, no locks, no hand-offs
     (the `current_thread` case);
   - **N > 1**: each **user** (with its networks, upstream connections and
     message store) is pinned to loop `user_id % N`; each **connection** is
     pinned to the loop that accepted it (listeners distribute accepted
     sockets round-robin). A downstream connection and its user may live on
     different loops, like a Tokio connection task and the user actor in
     soju-rs; they talk through **per-loop inboxes** (a mutex-protected
     queue drained by a `uv_async_t`) carrying owned messages. No object is
     ever touched from another loop: there are no locks around object
     state, only around the inboxes and the few process-wide tables
     (user directory, metrics counters with atomics, the live-object list
     of rule 11).
   Two other kinds of work leave the loops:
   - the **DB thread** (one dedicated thread owning the SQLite connection,
     matching the Rust `Mutex<Connection>` serialization), and
   - **CPU jobs** on the libuv threadpool (bcrypt, certfp key generation,
     Web Push encryption), limited like soju-rs's `cpu_work`.
   Jobs and inbox messages take **deep-copied inputs** and return **freshly
   allocated outputs**; they never touch another thread's objects. Results
   come back to the submitting loop through `uv_async_t` / `uv_queue_work`
   after-callbacks. TSan runs every integration test with N = 1 and N = 4
   (`SOJU_LOOPS` overrides N for tests).
2. **Objects are addressed by ID across async boundaries, never by pointer.**
   The Rust code already does this (`dc_id`, `conn_id`, `net_id`,
   `runner_id`). A completion callback looks the ID up in its owner's table;
   a missing ID means "the target died meanwhile" and the result is freed and
   dropped. This removes the classic use-after-free where a DB reply arrives
   after the client disconnected.
3. **Explicit ownership in names.** Functions that take ownership end in
   `_take` or document it in the prototype comment; returned pointers are
   owned by the caller unless the function name starts with `get_`/`peek_`.
   `*_free(NULL)` is always a no-op. Every struct has `x_init`/`x_destroy`
   (embedded) or `x_new`/`x_free` (heap).
4. **Single exit cleanup.** Functions with more than one resource use the
   `goto out;` pattern; locals that own memory may use
   `__attribute__((cleanup))` helpers (`AUTOFREE`, `AUTO_STR`).
5. **Allocation failure aborts.** `xmalloc`/`xcalloc`/`xrealloc`/`xstrdup`
   abort on OOM (Rust does the same). No code path handles NULL from our own
   allocator; errors from *libraries* are still handled.
6. **No raw string handling in feature code.** Use `str_t` (pointer + length,
   borrowed) and `strbuf_t` (owned, growable, always NUL-terminated). No
   `strcpy`, `strcat`, `sprintf`, `strtok`, `gets`, VLAs, `alloca`. All
   formatting goes through `strbuf_printf`, which checks `vsnprintf` results.
7. **Per-message arenas.** A parsed IRC message (prefix, command, params,
   tags) lives in one arena block; it is freed in one call after dispatch.
   Anything kept longer is copied out explicitly (`msg_clone`). This turns
   hundreds of tiny allocations per line into one, and makes "kept a pointer
   into a freed message" an ASan-visible UAF instead of silent corruption.
8. **libuv handle discipline.** A handle is freed only in its `uv_close`
   callback. Every connection has an explicit state machine
   (`OPEN → CLOSING → CLOSED`) and refcount held by: the handle, each
   in-flight write request, each pending TLS operation. The struct is
   freed when the refcount reaches zero *and* the close callback has run.
9. **Bounded queues, with the same limits as soju-rs.** Since v0.99.16 the
   Rust code bounds its queues too, and the C port uses the same numbers:
   the write queue keeps a byte counter and closes connections with more
   than 16 MiB queued; a downstream or upstream stops reading while 64 of
   its messages wait for the user's event queue (TCP flow control, events
   stay in order per connection); queued upstream commands (WHO, WHOIS,
   LIST, ...) are capped at 64 per type; CPU jobs (bcrypt, key generation)
   go through a limiter of one running job per CPU and at most 64 waiting,
   and one certfp generation per user.
10. **C23, using the features that catch bugs.** The code is written in
    ISO C23 (`-std=c23`), built with the latest GCC and the latest Clang
    (section 3.2):
    - `<stdckdint.h>` (`ckd_add`, `ckd_mul`) for every size calculation
      fed to an allocator or `memcpy` (buffer growth, base64/hex lengths,
      PROXY v2 lengths, WebSocket and SQLite blob sizes). Unchecked `a * b`
      on sizes is a review rejection;
    - `[[nodiscard]]` on every function returning an error or an owned
      pointer, so an ignored error or leaked return value fails to build
      under `-Werror`;
    - `nullptr` instead of `NULL`, `bool`/`true`/`false` as keywords,
      `static_assert` for struct layout assumptions (PROXY v2 header, table
      sizes);
    - `constexpr` for constants that used to be Rust `const`s (timeouts,
      limits such as `MAX_LINE_LENGTH`, `BACKLOG_LIMIT`), instead of macros;
    - `typeof` in the container macros (`vec`, `map`), so they are
      type-checked;
    - `[[gnu::cleanup(fn)]]` (standard attribute syntax for the GNU
      extension) for the `AUTOFREE` helpers of rule 4;
    - `#embed` to put `sqlite_schema.sql` and the help texts into the binary
      (replacing Rust's `include_str!`);
    - `[[fallthrough]]`, `[[maybe_unused]]` in place of comments/casts.
11. **Refcounted objects are tracked in debug builds.** `REF_NEW(type)`
    registers the object in a global live-object list with its type name and
    allocation site; `ref_dump_live()` runs at shutdown and fails the process
    (exit code 70) if anything is left.

### 2.2 Build modes

All built by `tools/ci.sh` from the same CMake tree. Dependencies are vendored
and built from source in every mode (needed for MSan, and keeps versions
pinned).

| Mode       | Compiler flags | Allocator | Purpose |
|------------|----------------|-----------|---------|
| `debug`    | `-O0 -g3 -ftrivial-auto-var-init=pattern` | mimalloc with `MI_DEBUG=3` (guard bytes, poison on free) | everyday dev |
| `asan`     | `-O1 -g -fsanitize=address,undefined -fno-omit-frame-pointer -fno-sanitize-recover=all` | system malloc (mimalloc off: ASan must own malloc) | UAF, overflow, leaks (LSan), UB |
| `msan`     | `-fsanitize=memory -fsanitize-memory-track-origins=2`, deps rebuilt with MSan, system OpenSSL replaced by an MSan build of the same version (`no-asm`) | system malloc | uninitialised reads |
| `tsan`     | `-fsanitize=thread` | system malloc | DB thread / threadpool handoff races |
| `valgrind` | `-O1 -g`, mimalloc with `MI_TRACK_VALGRIND=ON` | mimalloc | second opinion on leaks/uninit without recompiling deps |
| `release`  | `-O2 -D_FORTIFY_SOURCE=3 -fstack-protector-strong -fstack-clash-protection -fPIE -pie -Wl,-z,relro,-z,now` | mimalloc `MI_SECURE=ON` | shipped binaries (glibc and musl, both dynamic) |
| `fuzz`     | `-fsanitize=fuzzer,address,undefined` | system malloc | libFuzzer harnesses |

Warnings in every mode: `-std=c23 -Wall -Wextra -Wpedantic -Wshadow
-Wformat=2 -Wformat-truncation=2 -Wvla -Wnull-dereference -Wimplicit-fallthrough
-Wstrict-prototypes -Wmissing-prototypes -Wcast-qual -Werror`. Each mode is
built with both the latest GCC and the latest Clang where the compiler
supports it (matrix in section 3.2); GCC builds also add `-fanalyzer` in a
dedicated CI job.

### 2.3 Layers of testing

Each layer runs under `asan`, then `valgrind`; parsers and protocol code also
under `msan`; anything crossing threads under `tsan`.

**L1 — Unit tests (one per Rust unit test, plus more).**
The Rust tree has 165 `#[test]`/`#[tokio::test]` unit test functions
(v0.99.23; 109 at v0.99.15). Each gets a C
counterpart with the same name in `tests/unit/`, tracked in
`tests/PORTED_TESTS.md` (Rust path → C test). Framework: **cmocka**
(vendored, pinned like the other deps):
- one test binary per module (`tests/unit/test_irc_message.c`, ...), each
  test registered with CTest separately using `cmocka_set_test_filter()`, so
  every test runs in its own process and a crash or leak names that test;
- group `setup`/`teardown` build and destroy fixtures; every teardown asserts
  `ref_live_count() == 0`;
- mocking with `will_return()`/`mock()` and linker `--wrap` for the I/O and
  clock seams (`uv_now`, `getaddrinfo`, `RAND_bytes` for deterministic Web
  Push/VAPID vectors);
- in the `debug` build, `xmalloc` routes through cmocka's `test_malloc` in
  test binaries, so cmocka's own leak and buffer-overrun checks run as well
  as mimalloc's (the sanitizer builds turn this off and rely on ASan/LSan);
- output as TAP for the terminal and JUnit XML
  (`CMOCKA_MESSAGE_OUTPUT=xml`) for CI reports.
The L2 integration tests and L3/L4 torture loops use the same cmocka runner.

**L2 — Integration tests (ports of `tests/server.rs`).**
`tests/server.rs` has 17 scenarios, each run on both Tokio runtimes
(broadcast, chat history memory/db, statusmsg, malformed upstream messages, CAP REQ removal, labeled-response ACK,
SASL errors, CHATHISTORY params/limit, read marker, clock skew, multiline,
BouncerServ for other users, blocking work, MONITOR invalid targets,
multiplex, multiplex unsupported). They
run in-process: the server on a libuv loop, a fake upstream and test clients
over `socketpair()`/loopback. Each scenario runs in two DB modes:
- *async*: real DB thread (catches handoff bugs; also run under TSan),
- *delayed*: DB thread sleeps a random 0–20 ms per job (widens race windows
  between "job in flight" and "connection closed").
Teardown asserts: `uv_loop_close() == 0` (no leaked handles, `uv_walk` dumps
any offender), live-object table empty, LSan/valgrind clean.

**L3 — Lifecycle / teardown torture tests (`tests/lifecycle/`).**
These target the bugs C ports actually ship: dying objects with pending work.
Each is a loop over an injection point:
- *Disconnect at every byte*: record a full client session (registration,
  CAP, SASL, JOIN, CHATHISTORY, BouncerServ); replay it N times, closing the
  client socket after byte k for k = 0..len. Same for the upstream side
  (upstream dies mid-ISUPPORT, mid-NAMES, mid-batch, mid-SASL).
- *Disconnect at every event*: hooks in the user event dispatcher let a test
  close the downstream/upstream right before event #k is processed.
- *Pending-job races*: close a client while each kind of DB job is in flight
  (store message, load history, update receipts, create network, certfp
  generation, bcrypt check, Web Push send, OAuth2 introspection, Turso batch commit).
- *Multiplex*: close the bouncer connection, or `BOUNCER DETACH`, while each
  virtual connection is in its registration burst or has jobs in flight.
- *Admin races*: `user delete` / `network delete` / `user update -enabled
  false` while that user has connected clients and connecting upstreams.
- *Reload races*: SIGHUP (config, TLS cert, MOTD) while TLS handshakes and
  WebSocket upgrades are in progress.
- *Shutdown races*: SIGTERM with full write queues, during TLS handshakes,
  during vacuum, with timers armed (auto-detach, regain-nick, reconnect
  backoff, registration timeout).
- *Slow reader*: a client that never reads; assert the write timeout
  (10 s, `WRITE_TIMEOUT`) or the queue cap closes it and memory returns to
  baseline.

**L4 — Fault injection.**
- *I/O*: an `io_fault` shim (compiled into `debug`/`asan`) makes the k-th
  read/write/accept/connect/getaddrinfo fail with a chosen `UV_E*`.
- *SQLite*: a VFS shim returning `SQLITE_IOERR`/`SQLITE_FULL`/`SQLITE_BUSY`
  on the k-th operation; the server must log, reply with the same error the
  Rust code sends, and leak nothing.
- *TLS*: truncated handshakes, bad certificates, renegotiation attempts,
  close_notify vs. TCP RST, peers sending garbage after the handshake.
- *Library errors*: OpenSSL `EVP_*` failures forced via a provider stub in
  the Web Push and certfp tests.
- *Turso*: the mock Platform/Hrana server (port of the Rust independent
  mock, `src/database/turso/tests.rs`) drops responses, closes streams,
  returns malformed JSON, lost COMMIT replies and hosted size/processing
  limits on the k-th request.

**L5 — Fuzzing (`fuzz/`, libFuzzer + ASan/UBSan, MSan variant).**
Stateless harnesses, each with a seed corpus extracted from the compat
transcripts and Rust unit tests:

| Harness | Target | Property checked |
|---------|--------|------------------|
| `fuzz_irc_line` | IRC line parse + format | no crash; `parse(format(parse(x))) == parse(x)` |
| `fuzz_tags` | tag escape/unescape | round trip |
| `fuzz_scfg` | config file parser + `config_load` | no crash, no leak on error paths |
| `fuzz_proxyproto` | PROXY v1/v2 header | never reads past input; length checks |
| `fuzz_ws_frame` | our WebSocket text handling (line split, CR/LF, UTF-8) | matches Rust rules |
| `fuzz_utf8` | lossy decode / truncate at char boundary | output always valid UTF-8, ≤ input·3 |
| `fuzz_msgid` | msg ID parse/format | round trip |
| `fuzz_chathistory` | CHATHISTORY params, timestamps | no crash |
| `fuzz_isupport` | ISUPPORT / CHANMODES / PREFIX parsing | no crash |
| `fuzz_multiline` | multiline batch assembly | limits respected |
| `fuzz_whox` | WHOX field parse | no crash |
| `fuzz_url` | `parse_url`, `split_host_port`, Forwarded / X-Forwarded-For | no crash |
| `fuzz_sasl` | AUTHENTICATE chunking + PLAIN decode | no crash |
| `fuzz_ident` | identd query parse | no crash |
| `fuzz_json` | OAuth2 discovery/introspection, Web Push JSON | no crash |
| `fuzz_hrana` | Hrana pipeline responses (values, blobs, batons, errors) | no crash; malformed replies rejected |
| `fuzz_multiplex` | `soju.im/network` tag routing, `BOUNCER ATTACH`/`DETACH` | no crash |
| `fuzz_webpush` | encrypt then decrypt with test keys | round trip |

Stateful harnesses (the important ones for UAF):
- `fuzz_downstream_session`: input is a sequence of client lines plus
  "control bytes" (disconnect, upstream says X, upstream dies, timer fires,
  DB job completes). Runs against an in-memory server with a scripted fake
  upstream and a synchronous in-memory DB. After each input: full teardown,
  live-object table must be empty.
- `fuzz_upstream_session`: same, from the upstream side (hostile IRC server).

**Differential fuzzing against the Rust oracle.** A small Rust crate on
`main` (`tests/oracle/`, not shipped) exposes the Rust parsers (IRC line,
tags, scfg, msg ID, casemapping, PROXY) through a stdin/stdout protocol.
`fuzz/diff_*` feeds the same input to both and fails on any output difference.

Budgets: every PR runs each harness for 60 s; nightly runs 1 h each with
corpus minimisation, crashes filed as regression tests in `tests/regress/`.

**L6 — Protocol compatibility under sanitizers.**
The Python suites from `main` (`tests/compat/`: `diffharness.py`,
`ircv3spec.py`, `plumbing.py`, gamja Playwright) run against the C binary in
the `asan` build, then under valgrind (timeouts ×20). Three-way diff: Go soju,
soju-rs, soju-c — transcripts of soju-c must match soju-rs exactly (modulo
the existing normalisation); any Go/Rust difference already accepted stays
accepted.

**L7 — Logical leaks and soak.**
LSan only sees unreachable memory; "reachable but never freed" (maps never
pruned, pending-command queues growing) needs soak tests:
- `tools/soak.sh`: 2 ergo instances, 200 synthetic users, 2,000 clients
  connecting/disconnecting, channels joined/parted, history fetched, for 24 h
  (nightly: 2 h).
- Sample RSS and `mi_stats` (and the debug live-object counters per type)
  every minute; fail if the steady-state slope exceeds a threshold after
  warm-up. Run once under `heaptrack` to attribute growth.
- Compare peak RSS against soju-rs on the same workload (target: within
  ±30%).

**L8 — Static analysis.**
clang-tidy (`bugprone-*`, `cert-*`, `clang-analyzer-*`, `misc-*`,
`readability-else-after-return` off), `scan-build`, GCC `-fanalyzer`,
cppcheck. New findings fail CI; suppressions carry a justification comment.

### 2.4 Shutdown must free everything

Clean shutdown frees every allocation (no "the OS will clean up"), so leak
checkers produce zero noise and a single leaked byte is a failure:
stop listeners → stop users (each closes downstreams and upstreams) → drain
DB thread → cancel HTTP client requests → `uv_walk` closes stragglers (and, in test
builds, reports them as bugs) → `uv_loop_close` must return 0 →
`sqlite3_close` must return `SQLITE_OK` (no unfinalized statements) →
`OPENSSL_cleanup()` → `ref_dump_live()`. Suppression files
(`tests/lsan.supp`, `tests/valgrind.supp`) start empty; each entry needs a
comment linking the upstream library bug.

### 2.5 CI gates

| Gate | When | Blocks merge |
|------|------|--------------|
| build all modes × GCC and Clang × 2 targets, `-Werror` | every push | yes |
| L1 + L2 under asan, msan, tsan | every push | yes |
| L1 + L2 under valgrind | every push | yes |
| L3 + L4 under asan | every push | yes |
| L5 60 s per harness | every push | yes |
| L6 compat suites under asan | every push | yes |
| L8 static analysis | every push | yes |
| L5 1 h, L6 under valgrind, L7 2 h soak | nightly | opens an issue |
| L7 24 h soak | before each release | yes |

---

## 3. Dependencies

Two kinds:
- **System libraries**: OpenSSL 3 and libxcrypt (plus libc), always linked
  **dynamically** from the distribution (runtime dependencies `libssl.so.3`,
  `libcrypto.so.3`, `libcrypt.so.2`). No binary is linked with `-static`,
  so OpenSSL security updates reach soju through the distro without a
  rebuild. OpenSSL is whatever the image ships (3.6.5 on Arch and Alpine
  edge on 2026-10-29); CMake requires ≥ 3.0 via `find_package(OpenSSL)`.
  Alpine edge does not package libxcrypt (checked 2026-10-22), so the musl
  build compiles the libxcrypt release that Arch ships at the time (4.5.2
  today) from its release tarball, checksum verified, and links it into
  the binary.
- **Vendored libraries** (everything else): pinned to the latest stable
  release of each at bootstrap time (versions verified against upstream tags
  then, recorded in `deps/VERSIONS`), vendored as tarballs with checksums,
  built static by CMake `ExternalProject` and linked into the (dynamic)
  binaries. This covers the **SQLite amalgamation** (like rusqlite's
  `bundled` feature), libuv, mimalloc (3.x), utf8proc, yyjson and cmocka.
  libcurl is a system library like OpenSSL (dynamic; built from source with
  MSan only, against the instrumented OpenSSL). Both targets
  get the same versions, and the MSan build can instrument them.

Sanitizer builds and the system libraries: they are not instrumented, so
the `msan` build (and only that one, test-only) compiles the same OpenSSL
(`no-asm`) and libxcrypt versions as the gnu image ships from source with
MSan. ASan, TSan and valgrind work with the
uninstrumented system library.

| Rust crate(s) | C replacement | Notes |
|---------------|---------------|-------|
| tokio (runtime, timers, signals, process) | **libuv** | one loop thread per CPU (one on a single CPU, like the `current_thread` flavor) + DB thread + threadpool |
| tokio-rustls, rustls, aws-lc-rs, webpki-roots, rustls-native-certs | **OpenSSL 3** (system, dynamic) | our own TLS-over-libuv layer using memory BIOs; system trust store via `SSL_CTX_set_default_verify_paths` |
| hyper, tokio-tungstenite | in-tree `http/request.c`, `http/websocket.c`, `server/http.c` on `net/conn` (decision 11) | HTTP/1.1 server, WebSocket, `/socket`, `/icon`, Prometheus |
| reqwest | **libcurl** (system, dynamic) multi interface on libuv (decision 12) | OAuth2, HTTP auth, Web Push; internal IPs refused at every connect |
| reqwest (blocking Turso worker), rusqlite overlay | **libcurl** easy interface on the DB thread (blocking, like the Rust worker), yyjson | Turso Hrana pipelines and Platform API; the DB thread blocks on its reply like the Rust HTTP worker; in-memory SQLite overlay for buffered messages |
| mimalloc | **mimalloc** | `MI_SECURE` in release |
| (Rust `str` UTF-8 handling) | **utf8proc** | validation, lossy decoding, char-boundary truncation, NFC where needed |
| rusqlite (bundled) | **SQLite amalgamation** (vendored) | compiled into the binary with `SQLITE_ENABLE_FTS5`, `SQLITE_THREADSAFE=2` (one DB thread owns the connection), `SQLITE_DQS=0`, `SQLITE_DEFAULT_FOREIGN_KEYS=1` if soju-rs relies on it (checked in phase 2); WAL; the same options as rusqlite's bundled build so behaviour matches |
| bcrypt | **libxcrypt** (system on Arch) | `crypt_gensalt_rn("$2b$", 10, ...)` + `crypt_rn` with a heap-allocated `struct crypt_data` per job (thread-safe, wiped with `explicit_bzero` after use); verifies `$2a$`/`$2b$`/`$2y$` hashes from Go soju and soju-rs |
| serde_json | **yyjson** (MIT) | read-only and mutable docs |
| aes-gcm, hkdf, p256, sha1, sha2, hex, base64, rand | OpenSSL EVP / `RAND_bytes` | AES-128-GCM, HKDF, ECDH/ECDSA P-256, SHA-1/256 |
| rcgen | OpenSSL `X509_*` | certfp self-signed certs (RSA 2048–8192, ECDSA, Ed25519) |
| chrono, time | in-tree `timeutil.c` | RFC 3339 with any fractional digits, `clock_gettime` |
| indexmap, ipnet, socket2, gethostname, libc | in-tree / POSIX | ordered map, CIDR matching |
| anyhow | in-tree `err_t` | error value with owned message string |
| (`cargo test`) | **cmocka** + CTest | unit/integration test framework, mocking, JUnit output |

Language: C23 (see rule 10 in 2.1). Vendored dependencies are built with
their own default standards; only our code is compiled as C23, and public
headers we include from them must compile cleanly under `-std=c23`
(checked in phase 0).

Build system: CMake ≥ 3.29 with Ninja (libuv, mimalloc,
utf8proc, yyjson and cmocka are CMake projects). Each build mode of 2.2 is a
preset in `CMakePresets.json` (configure, build and test presets, build tree
in `build/<compiler>-<mode>`, e.g. `gcc-asan`, `clang-msan`). Formatting is defined
by `.clang-format` and enforced on changed files by `/commit`.

### 3.1 Targets and Docker images

Linux x86_64 only, two targets, each built **and tested** natively in
Docker from freshly pulled bases with all packages upgraded
(`docker build --pull --no-cache`, `pacman -Syu` / `apk upgrade`), like
`scripts/build-release.sh` on `main`:

| Target | Image | Build |
|--------|-------|-------|
| `x86_64-linux-gnu` | `archlinux:latest` | dynamic glibc, OpenSSL, libxcrypt; vendored libraries linked in |
| `x86_64-linux-musl` | `alpine:edge` | dynamic musl and OpenSSL; libxcrypt and vendored libraries linked in |

Files: `docker/Dockerfile.gnu`, `docker/Dockerfile.musl`,
`tools/docker-test.sh <target> [mode...]` (configure, build, CTest, compat
suites inside the container), `scripts/build-release.sh` (both targets,
archives + `SHA256SUMS` in `dist/`, same layout as soju-rs releases).

Which build modes of 2.2 run on which target (the sanitizer runtimes don't
support musl):

| Mode | gnu | musl |
|------|:---:|:---:|
| `debug` (mimalloc debug, cmocka leak checks) | ✓ | ✓ |
| `release` | ✓ | ✓ |
| `asan` (ASan+UBSan+LSan) | ✓ | – |
| `ubsan-trap` (`-fsanitize=undefined -fsanitize-trap=all`, no runtime needed) | ✓ | ✓ |
| `msan` | ✓ | – |
| `tsan` | ✓ | – |
| `valgrind` | ✓ | ✓ (Alpine `valgrind`) |
| `fuzz` | ✓ | – |
| compat suites (L6) | ✓ (also under asan) | ✓ (release; valgrind nightly) |

### 3.2 Compilers

Every target is built and tested with **both the latest GCC and the latest
Clang** available in the freshly upgraded images (on 2026-10-22: GCC 16.2 and
Clang 23.1 in `archlinux:latest`; GCC and the newest `clangNN` package,
currently `clang23`, in `alpine:edge`). Dockerfiles install the newest
versions instead of hard-coding them, and `tools/docker-test.sh` prints the
compiler versions at the top of every log.

The two compilers warn about different things and have different
sanitizers, so both must build with `-Werror` and pass the tests:

| Mode | GCC | Clang |
|------|:---:|:---:|
| `debug`, `release`, `ubsan-trap` | ✓ | ✓ |
| `asan` (ASan+UBSan+LSan) | ✓ | ✓ |
| `tsan` | ✓ | ✓ |
| `msan` | – (Clang only) | ✓ |
| `fuzz` (libFuzzer) | – (Clang only) | ✓ |
| `valgrind` | ✓ | ✓ |
| static analysis | `-fanalyzer` | `scan-build`, clang-tidy |

Compiler-specific options (e.g. `-Wformat-truncation=2`,
`-Wstack-clash-protection`, `-fanalyzer` on GCC; `-Wthread-safety`,
`-Wcomma`, `-Wunreachable-code-aggressive` on Clang) are set per compiler in
CMake. Release archives are built with GCC; Clang release builds run the
same tests, and phase 11 compares their speed and size before choosing.

The rows of the target table above apply to each compiler, so the CI
matrix is 2 targets × 2 compilers × the modes allowed for each.

Everything runs on the gnu target. The musl target catches what differs
there: the smaller default thread stack (128 KiB, so the DB thread and
threadpool set stack sizes explicitly), musl's `getaddrinfo` and `crypt`
symbol clashes with the linked-in libxcrypt. The DB-thread and `uv_async`
handoff uses only `stdatomic.h` acquire/release or libuv/pthread primitives,
never plain flags, so TSan on gnu covers it for both targets.

**Spike risks (resolved in phase 0 before committing to the design):**
1.–4. libwebsockets on a foreign libuv loop, socket adoption after a PROXY
   header, vhost TLS contexts, and its HTTP client's DNS hook and timeouts.
   Resolved in phase 0, then made moot: phases 8 and 9 replaced
   libwebsockets with the in-tree HTTP server (decision 11) and libcurl
   (decision 12), and its spikes were removed.
5. libxcrypt bcrypt compatibility: hashes created by Go soju and soju-rs
   verify, and new `$2b$` hashes verify in both; the static libxcrypt build
   links on Alpine without clashing with musl's own `crypt`.

---

## 4. Architecture

```
                         ┌───────────────── loop thread (libuv) ─────────────────┐
 listeners ──accept──►   │ net/listener ─► net/proxyproto ─► net/tls (OpenSSL)   │
 (tcp/unix)              │        │                 └──► server/http (HTTP, WS) │
                         │        ▼                                             │
                         │ conn (line reader, write queue, rate limit)          │
                         │        ▼                                             │
                         │ downstream ──events──► user (per-user state, by ID)  │
                         │                         ▲     │                       │
                         │ upstream ◄──────────────┘     ▼                       │
                         │                         msgstore / service           │
                         └────────────┬──────────────────────┬─────────────────┘
                                      │ jobs (owned copies)  │ CPU jobs
                                      ▼                      ▼
                               DB thread (SQLite)     uv threadpool (bcrypt,
                                      │                keygen, webpush crypto)
                                      └── uv_async results back to loop ──┘
```

- Loops: N loop threads (rule 1), each with the diagram's components for
  the users and connections pinned to it; with N = 1 it is exactly the
  diagram. Cross-loop events (a downstream line for a user on another
  loop, a message to deliver to a connection on another loop) go through
  the destination loop's inbox.
- The Rust per-user actor (`User::run` + `mpsc` of `Event`) becomes a per-user
  struct whose event handlers are called directly on the user's loop thread. Events
  that were posted to a channel (timers, upstream messages, DB results) keep
  being *queued* per user and drained in a `uv_idle`/`uv_check` callback, so
  the handler order and re-entrancy match the Rust event loop (a handler
  never runs inside another handler).
- `server::blocking` (SQLite calls inside event handlers) is replaced by
  asynchronous DB jobs with continuations, except where the Rust code relies
  on the result synchronously; those call sites are listed during phase 2 and
  either restructured or run as a synchronous call on the loop thread against
  the DB thread's queue (blocking wait), matching current-thread Rust
  behaviour.

Source layout (mirrors the Rust modules so the port can be reviewed file by
file):

```
src/
  base/      alloc.c str.c strbuf.c vec.h map.c (casemapped + ordered) arena.c
             ref.c (debug live-object table) err.c log.c timeutil.c utf8.c
  irc/       message.c tags.c numerics.h casemapping.c genmsg.c multiline.c
             whox.c ircutil.c
  config/    scfg.c config.c flags.c
  db/        db.c dbthread.c sqlite_schema.sql migrations.c users.c networks.c
             messages.c receipts.c webpush.c routing.c message_buffer.c
  db/turso/  turso.c hrana.c platform.c controller_schema.sql
             tenant_schema.sql tenant_user_migration.sql
  net/       listener.c conn.c tls.c proxyproto.c identd.c rate.c
  http/      request.c websocket.c client.c response.c
  downstream/ caps.c registration.c sasl.c send.c sync.c welcome.c multiplex.c
             cmds/*.c
  upstream/  upstream.c connect.c registration.c channels.c users.c messages.c
             commands.c
  user/      user.c network.c channels.c delivered.c events.c upstream_conn.c
             webpush.c
  msgstore/  msgstore.c memory.c db.c bare.c turso.c
  service/   service.c auth.c channel.c duration.c network.c server.c user.c
  auth.c certfp.c webpush.c metrics.c server.c
  main/      soju.c sojuctl.c
tests/ unit/ integration/ lifecycle/ regress/ PORTED_TESTS.md
fuzz/ deps/ tools/ doc/
```

---

## 5. Phases

Every phase ends with: its tests passing in all build modes of 2.2, no
sanitizer or valgrind findings, its fuzzers running clean for 10 min, and
`tests/PORTED_TESTS.md` updated. No phase starts feature work before the
previous phase's exit criteria hold.

**Phase 0 — Bootstrap and spikes**
CMake tree, vendored deps, all build modes, CI skeleton, cmocka + CTest wiring, fuzz
skeleton, `ref` live-object tracking, `xmalloc`. Resolve spike risks 1–5 of
section 3 with throwaway programs that pass ASan + valgrind with zero leaks.
Docker images and `tools/docker-test.sh` for both targets with both
compilers.
Exit: `hello` binary (linking every dependency) built and tested in every
target × compiler × mode cell of sections 3.1–3.2; spike report in
`doc/spikes.md`.

**Phase 1 — Base and pure logic** (no I/O)
`base/*`, `irc/*` (parser, formatter, tags, casemapping, ISUPPORT, multiline,
WHOX, genmsg), `scfg`, `config`, `flags`, `timeutil`, UTF-8 rules, msg IDs,
`rate` backoff maths, URL/host:port parsing, Forwarded parsing.
Tests: Rust unit tests of these modules, all stateless fuzzers + differential
fuzzers vs. the Rust oracle. This phase has the highest bug density per line;
it gets the longest fuzzing before moving on (≥ 8 CPU-hours per harness).

**Phase 2 — Database**
Schema (local SQLite schema 33: rowid `Message` table, `MessageTextIndex`,
`MessageIdHighWater`), migrations (from every historical upstream version),
all queries, DB thread with job queue, bcrypt via the CPU-job limiter.
Message IDs are never reused (high-water mark recorded before deletes);
expiry deletes 2000 rows at a time with a 10 ms pause.
Tests: port `src/database/sqlite/tests.rs`; open databases created by Go soju
v0.11 and by soju-rs, run migrations, compare `sqlite3 .dump` output with the
Rust result; SQLite fault-injection VFS; TSan on the job handoff;
`sqlite3_close` returns `SQLITE_OK` at every teardown.

**Phase 3 — Connections**
Listeners (TCP/Unix, socket modes 0600/0775), PROXY v1/v2, TLS server/client
over libuv (client-cert capture, pinned upstream fingerprints, ALPN `irc`),
line reader (32 KiB limit, CR/LF rules), write queue with batching, write
timeout, rate limiter with SAFERATE flag, identd.
Tests: port `conn.rs`, `proxyproto.rs`, `identd.rs`, `rate.rs`, `tls.rs`
tests; L3 disconnect-at-every-byte on raw and TLS connections; L4 I/O faults;
TLS torture cases.

**Phase 4 — Downstream**
Registration (`username/network@client`), CAP 302, SASL (PLAIN, EXTERNAL,
OAUTHBEARER), welcome, caps, labeled-response, batches, send helpers, sync,
`soju.im/multiplex` (connection views: a tagged writer that can be closed
without closing the connection; per-view batches, labels and multiline).
Tests: unit tests of `downstream/*`; `fuzz_downstream_session` starts here.

**Phase 5 — Upstream**
Connect (with backoff and jitter, TCP keep-alive), registration (60 s
timeout once connected; CAP END on SASL NAK), SASL client, caps, channels,
users, messages, commands (64 queued per type, only the command name logged
on timeout), regain-nick and auto-detach timers. Pinned fingerprints match
the leaf certificate only.
Tests: port `upstream/server_tests.rs`; `fuzz_upstream_session`.

**Phase 6 — User and networks**
User event queue, networks, channels, delivery receipts, msgstore
(memory/db/bare), CHATHISTORY, read markers, MONITOR, multiline forwarding.
Tests: all of `tests/server.rs` (L2) in both DB modes; L3 pending-job races.

**Phase 7 — BouncerServ**
All service commands (`network`, `channel`, `certfp`, `sasl`, `user`,
`server`, `help`), deferred commands, admin restrictions.
Tests: port service tests; L3 admin races.

**Phase 8 — HTTP and WebSocket**
`/socket`, `/icon`, origin checks, Forwarded/X-Forwarded-For with trusted
proxies, Prometheus metrics, PROXY on HTTP listeners.
Tests: port `listen.rs` tests (incl. large WebSocket lines); gamja
Playwright suite; L3 reload races during upgrades.

**Phase 9 — Auth and Web Push**
HTTP and OAuth2 authenticators, Web Push (VAPID JWT, RFC 8291 encryption,
expired-subscription handling, internal-IP refusal, at most 25 subscriptions,
per-user cache of networks with subscriptions) over the libcurl HTTP client.
Tests: port `auth.rs`, `webpush.rs` tests (decrypt round trip, VAPID, internal
IPs); local mock HTTP servers.

**Phase 9b — Turso Cloud storage**
Hrana client (pipelines, batons, replication indexes, unpadded base64 blobs,
named parameters, never replaying ambiguous commits), Platform API
provisioning (25-character base36 names, `provisioning`/`ready`/`deleting`
states, recovery at startup), controller and tenant schemas (tenant schema
33: `WITHOUT ROWID` `Message` keyed `(target, time, id)`, resumable copy
migration), routing of queries to the user's database, message write
batching (chathistory-limit rows, 1 s partial flush, 512 KiB body budget,
splitting on hosted limits, bisection of constraint-rejected rows, shutdown
drain), `sojuctl turso create-admin`.
Tests: port `src/database/turso/tests.rs` and `cost_tests.rs` against a C
port of the Platform/Hrana mock; L4 Turso faults; L3 network deletion and
shutdown with buffered writes.

**Phase 10 — Process glue**
`soju` main (flags, `-version`, SIGHUP reload, SIGTERM/SIGINT shutdown,
message-expiry + vacuum schedule, inactive-user job), `sojuctl` over
`unix+admin`, man pages.
Tests: L3 shutdown races, L4 everything, `plumbing.py`.

**Phase 11 — Hardening and release**
Full L6 three-way compat under ASan and valgrind, 24 h L7 soak, performance
comparison with soju-rs, release builds (x86_64 glibc and musl, dynamically linked), README "Differences" section for the C port.

---

## 6. Definition of done

- All 165 Rust unit tests and 17 integration scenarios ported and passing.
- `tests/compat` differential, IRCv3 spec, plumbing and gamja suites pass,
  with soju-c transcripts identical to soju-rs.
- Clean under ASan/UBSan, MSan, TSan and valgrind across L1–L4 and L6.
- No fuzzer finding open; each harness has ≥ 24 CPU-hours total.
- 24 h soak with no RSS growth after warm-up and no live objects at
  shutdown.
- An existing `soju.db` from soju-rs works unchanged, and back again.

## 7. Decisions

1. Allocator: **mimalloc**.
2. Write-queue cap (rule 9): **kept**; soju-rs adopted the same 16 MiB cap
   in v0.99.16, so it is no longer a divergence.
3. JSON: **yyjson**.
4. Binaries are named **`soju`** and **`sojuctl`**. Test scripts may copy
   them under other names (e.g. `soju-c`) to run side by side with soju-rs
   and Go soju in the compat harness.
5. bcrypt: **libxcrypt** (system library); SQLite: **bundled** amalgamation;
   TLS/crypto: **system OpenSSL, dynamically linked**; no `-static` binaries.
6. Targets: Linux x86_64 only, `gnu` and `musl`, built and tested in
   `archlinux:latest` and `alpine:edge` (section 3.1). No aarch64.
   Release archives document their runtime packages (`openssl`,
   `libxcrypt` on glibc).
7. Compilers: latest GCC and latest Clang, both required to pass
   (section 3.2).
8. Turso Cloud storage and `soju.im/multiplex` are **in scope**: they are
   documented soju-rs features (README "Additions").
9. Threads: one libuv loop per CPU, a single loop on a single CPU, like
   soju-rs's choice between Tokio's `current_thread` and `multi_thread`
   runtimes (rule 1).
10. NUL bytes in received lines become U+FFFD (soju-rs keeps them; C
    strings can't hold them). Values decoded from data where NUL may occur
    (e.g. message ID targets) are kept length-aware instead.
11. The HTTP server and WebSocket are **in-tree** (phase 8), not
    libwebsockets: `http/request.c` (HTTP/1.1 request heads),
    `http/websocket.c` (RFC 6455 framing) and `server/http.c` (routes),
    on top of `net/conn` with raw and WebSocket framings. HTTP and IRC
    connections then share one TLS layer, write queue, byte cap, input
    backpressure and lifetime model (rule 8), and a WebSocket connection is
    an ordinary `struct conn` for the downstream code. lws's adoption API
    can't take bytes already read and would need one context per loop for
    rule 1. Both parsers have fuzz harnesses (`fuzz_ws_frame`,
    `fuzz_http_request`).
12. The HTTP client is **libcurl** (phase 9), the system library linked
    dynamically like OpenSSL, through its multi interface on each loop
    (`uv_poll` and a `uv_timer`), instead of libwebsockets' client: Web
    Push endpoints are user-supplied URLs, so responses come from servers
    that may be hostile, and libcurl's HTTP/1.1 parsing, redirect rules
    (reqwest-like by default: POST becomes GET on 301/302/303, credentials
    stay on the original host) and per-connect address checks
    (`CURLOPT_OPENSOCKETFUNCTION` refuses internal IPs after DNS and on
    redirects) are long proven. libwebsockets is no longer a dependency.

## 8. Changes on `main` since v0.99.15

Folded into the sections above; listed here so the next rebase of this plan
starts from `c350574` (v0.99.23).

| Commit | Change | Plan impact |
|--------|--------|-------------|
| `8780ccd` | Audit fixes: leaf-only fingerprint pinning, 16 MiB output cap, 64-message input backpressure, 64 queued commands per type, CPU-job limiter, linear tag parsing, never-reused message IDs (`MessageIdHighWater`), 60 s upstream registration timeout, TCP keep-alive, bcrypt upgrade preservation, `sojuctl` quoting | rule 9, phases 1–3, 5, 10 |
| `30710ff` | Web Push subscription cache; chunked expiry deletes | phases 2, 9 |
| `e0957f3` | `soju.im/multiplex` | goals, phase 4, L2/L3/L5 |
| `e3a3349` | `MessageTextIndex` (schema 31) | phase 2 |
| `d478f60`…`c350574` | Turso Cloud storage, write batching, schemas 32–33, clustered tenant `Message`, SASL NAK and registration-timer fixes | goals, deps, layout, phase 9b, L4 |

## Implementation validation: phases 4–7

Phase 4: welcome/sync and multiplex lifecycle socket tests, downstream
session seed replay, GCC/Clang debug and ASan/UBSan/LSan suites, clang-tidy,
and a 60-second libFuzzer run. Long fuzzing and the full target/mode matrix
are still required by the phase exit criteria.

Phase 5: upstream protocol and TCP/DNS lifetime tests, GCC/Clang debug and
ASan suites, clang-tidy, and a 60-second upstream-session libFuzzer run.
Phase 4 additionally passed GCC/Clang release, UBSan and TSan, and Clang
MSan. Valgrind could not configure because the executable is not installed.

Phase 6: 15 of 17 Rust server scenarios run through real sockets in four
configurations (1/4 loops × normal/delayed DB jobs), plus lifecycle and
memory-backlog checks. GCC/Clang debug, ASan/UBSan/LSan and TSan pass.
The two BouncerServ scenarios are covered by the phase 7 handlers below. Local SQLite
message append/history, receipt and network/channel updates use blocking
DB-thread calls with copied inputs, matching Rust's synchronous local
storage sections; authentication and account creation use continuations.

Phase 7: the complete BouncerServ command tree, abbreviated lookup and help,
network/channel persistence, SASL and device certificates, user administration
and server commands. Password hashing and certificate generation use the CPU
limiter; cross-user commands and replies use owned events and IDs. Commands
retain their initiating authority across `user run`, bound nesting and waiting,
and tolerate disconnect, deletion and shutdown. User/network IDs retain local
high-water marks across deletes so deferred work cannot reach a replacement
account or network. All three Rust service unit tests and both remaining server
scenarios are ported, alongside command, global-callback and admin race tests.
GCC/Clang debug, ASan/UBSan/LSan and TSan suites and clang-tidy pass.

## Implementation validation: phase 8

HTTP listeners (`wss`/`https`, `ws+insecure`/`http+insecure`, Unix and
Prometheus) go through `net/conn` like IRC ones: TLS with ALPN `http/1.1`,
no PROXY header (like soju-rs), raw framing for HTTP/1.1 and WebSocket
framing after the upgrade (decision 11). Routes: `/socket` (handshake checks
and error texts of soju-rs, Origin check, `text.ircv3.net` subprotocol,
Forwarded/X-Forwarded-For and `Client-Cert` from trusted proxies), `/icon`
and the Prometheus metrics. Keep-alive and pipelining work; request bodies
are skipped; the header read timeout is 30 s, including idle keep-alive
connections. WebSocket frames and messages are limited to 32 KiB, pings are
answered, a normal close frame is an EOF answered with a close frame,
other status codes are errors. The three remaining phase 8 Rust tests are
ported, with C-only codec, parser, framing-switch and end-to-end HTTP tests
(an IRC session over WebSocket against the server fixture, in all four
integration configurations). GCC/Clang debug and ASan/UBSan/LSan suites,
clang-tidy, and 60-second libFuzzer runs of both new harnesses pass.

## Implementation validation: phase 9

Part 1, authentication: the HTTP client is libcurl's multi interface on
each loop (decision 12; libwebsockets and its phase 0 spikes are gone), with
a timeout covering the whole exchange, reqwest-like redirects, internal
addresses refused at every connect for Web Push, and cancellation of every
request at shutdown. PLAIN goes to the internal database, an HTTP endpoint
(Basic credentials, Forwarded/X-Forwarded-For) or OAuth 2.0 token
introspection; OAUTHBEARER uses the same introspection, and OAuth 2.0
servers are discovered (RFC 8414) when the auth directives are loaded. The
four `auth.rs` tests are ported, with client tests against a threaded mock
HTTP server and an OAUTHBEARER login end to end. GCC/Clang debug,
ASan/UBSan/LSan, TSan and Clang MSan (libcurl built from source,
instrumented) pass, and clang-tidy is clean.

Part 2, Web Push: RFC 8291 encryption and VAPID (RFC 8292) with OpenSSL
(P-256 keys, ECDH, HKDF-SHA-256, AES-128-GCM, ES256), the VAPID key pair
generated on first start, `VAPID` in ISUPPORT with `soju.im/webpush`,
`WEBPUSH REGISTER`/`UNREGISTER` with soju-rs's limits (25 subscriptions per
network, 4 pending checks, re-check after 24 h) and asynchronous test
notifications, highlights and direct messages pushed after 5 s unless read
(muted and blocked targets excepted), `MARKREAD` pushes dismissing them,
expired subscriptions deleted and failing ones pruned after 30 days. The
encryption is done on the loop (a key generation, an ECDH and a signature
per notification) rather than through the CPU-job limiter. The five
`webpush.rs` tests are ported, with integration tests of the command and of
notifications through the server.
