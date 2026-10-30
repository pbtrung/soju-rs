# Audit and refactor plan

Status: **in progress** (phase A). Scope: the C port on `c-port` after
phase 11 (`src/` ≈ 36k lines, `tests/` + `fuzz/` ≈ 14k lines), qualified
against soju-rs v0.99.24 (doc/phase11-wip.md). Read with
[PLAN.md](../PLAN.md) (sections 2 and 4) and
[conventions.md](conventions.md); this plan doesn't change their rules, it
checks that the code follows them and restructures what has outgrown them.

Why now: phases 8–11 (HTTP/WebSocket, auth, Web Push, Turso, process glue,
three-way compatibility) are done, and grew the large files further. The
plan was written at phase 7; this revision updates the numbers to the
current tree. The differential, IRCv3 and plumbing suites now pass, so
they're the safety net for the refactor: a split must keep all of them
green (tests/compat/run-diff.sh 25/25, plumbing 20/20).

---

## 1. Ground rules

1. **Refactors don't change behaviour.** A split or move commit contains no
   logic changes; it passes the same tests, fuzz seeds and oracle runs
   before and after. Fixes go in separate commits.
2. **Every finding gets a test.** A bug fix lands with the cmocka test, fuzz
   seed or oracle case that fails without it. A finding that can't be
   tested is written down in the module header with the reason.
3. **soju-rs stays the reference.** A "bug" whose behaviour matches soju-rs
   is checked against `git show main:src/...`. If soju-rs is wrong too, the
   fix is recorded as a deliberate difference (README "Differences" list,
   PLAN.md phase 11) rather than silently diverging.
4. **Every commit passes** `tools/ci.sh -j 4 gcc-debug gcc-asan clang-debug
   clang-asan`; commits touching threads also pass `gcc-tsan clang-tsan`.
5. Findings are tracked in `doc/audit-findings.md` (one row per finding: ID,
   module, dimension, severity, status, commit, test).

Severity scale:

| Level | Meaning |
|-------|---------|
| S1 | Remote crash, memory corruption, auth bypass, data loss |
| S2 | Leak, hang, wrong protocol behaviour visible to clients, local DoS |
| S3 | Divergence from soju-rs on an edge case, misleading error, perf cliff |
| S4 | Readability, naming, dead code, missing comment |

---

## 2. Phase A — Baseline and tooling (before touching code)

Measure first so the refactor and audit have something to compare against.

| Step | Output |
|------|--------|
| Add a `coverage` mode to `cmake/SojuMode.cmake` (clang `-fprofile-instr-generate -fcoverage-mapping`) and a `clang-coverage` preset; `tools/coverage.sh` produces an `llvm-cov` HTML report from CTest + fuzz seed replay | Line/branch coverage per file; list of untested functions |
| `tools/metrics.sh`: per-file line count, per-function length and cyclomatic complexity (`pmccabe` or `lizard`) | Baseline table committed in `doc/audit-findings.md` |
| One clang-tidy report run (not a gate) with checks `.clang-tidy` doesn't enable yet: `readability-function-cognitive-complexity`, `readability-function-size`, `performance-*`, `concurrency-*`; also re-review each check disabled there (`-bugprone-easily-swappable-parameters`, `-misc-no-recursion`, ...) and whether it still needs to be off | Triage list |
| `scan-build` / `clang --analyze` and GCC `-fanalyzer` over `src/` | Triage list |
| Run each existing fuzzer for 1 CPU-hour under ASan | Fresh crash/timeout list; corpus coverage per harness |
| Record oracle pass rate: `tools/oracle.sh` over all ops | Baseline |

Exit: baseline numbers recorded; triage lists filed as findings (most will be
S4 or false positives — mark them so and move on).

### 2.1 Baseline (2026-10-30, commit after 7153733)

Done: the `coverage` mode and `clang-coverage` preset, `tools/coverage.sh`
and `tools/metrics.sh` (lizard, from PATH or `build/venv`).

| Measure | Value |
|---------|-------|
| Unit + integration tests | 428, all passing |
| Line coverage of `src/` | 78.2% (5,835 of 26,774 lines uncovered) |
| Function coverage | 93.1% (110 of 1,587 functions never run) |
| Branch coverage | 63.4% |
| Files over 500 lines (`src/`, `tests/`, `fuzz/`) | 21 (11 in `src/`) |
| `src/` functions over 80 lines | 36 |
| `src/` functions with cyclomatic complexity over 15 | 104 |

Done: the clang-tidy report with the extra and disabled checks, triaged
in audit-findings.md (A-01 to A-08); nothing above S3.

Done: GCC 16 `-fanalyzer` over the 104 `src/` files with the gcc-debug
flags: no warnings (the analyzer was checked to report a planted leak).

Done: the oracle pass rate. `diff_oracle` replays its 146 seeds against
soju-rs at 6b9bf15 (v0.99.24, tools/oracle.sh): 146/146 match, every one
of the 24 ops has at least one seed.

Deferred: 1 CPU-hour per fuzzer (6 harnesses), run before phase B ends.

---

## 3. Phase B — Structural refactor (split large files)

### 3.1 Targets

**Hard limit: 500 lines per `.c`/`.h` file** in `src/`, `tests/` and
`fuzz/` (generated `*.inc` tables and vendored `deps/` excluded).
`tools/lint.sh` enforces the limit once phase B ends, so later work can't
reintroduce large files.

Function limits, chosen for readability (a function should fit on one
screen, and C error handling makes it 20–30% longer than the Rust it
mirrors):

| Lines | Rule |
|------:|------|
| ≤ 50 | Target for every function |
| 51–60 | Fine if it reads top to bottom with nesting depth ≤ 3 |
| 61–80 | Reviewed: split unless there's a reason, written in a comment |
| > 80 | Split, except flat dispatch (a `switch` or table whose cases are a few lines each) |

Also: nesting depth ≤ 3 (early returns and the `goto out` cleanup path
instead of nested `if`s) and cyclomatic complexity ≤ 15. Long functions
are broken up in the same series as their file's split. Splits follow real seams (Rust module, protocol
area, lifecycle versus I/O); a file isn't cut in half just to meet the
number. Line counts below are estimates taken from each file's section
boundaries.

Files over the limit today (2026-10-30):

| File | Lines | Split into (~lines) |
|------|------:|---------------------|
| ~~`src/downstream/downstream.c`~~ | 2026 | **Done.** Its header says it combines four Rust modules (`mod.rs`, `send.rs`, `caps.rs`, `registration.rs`); `handle_cap` 103 lines. New layout (PLAN.md section 4): `downstream.c` lifecycle, loop table, cap tables; `receive.c` receive dispatch and pending input while busy; `send.c` sending, batches, labeled-response; `caps.c` CAP LS/LIST/REQ with `handle_cap` split per subcommand, ISUPPORT; `util.c` username unmarshalling, NickServ credentials, network attrs; `registration.c` PASS/USER/NICK, `try_register`; SASL and AUTHENTICATE move into `sasl.c`; pre-registration METADATA SUB into `sync.c`. Private declarations go in a new `downstream/internal.h` |
| ~~`src/user/commands.c`~~ | 1183 | **Done** (`commands.c`, `cmd_util.c`, `cmd_channel.c`, `cmd_query.c`, `cmd_account.c`, `cmd_privmsg.c`, private `cmds.h`). Mirrors soju-rs `downstream/cmds/`: `commands.c` dispatch and BOUNCER (`user_handle_command` 204 lines becomes a command table); `channel_cmds.c` JOIN/PART/MODE/TOPIC/NAMES (basic.rs); `query_cmds.c` WHO/WHOIS/MONITOR and the WHO cache (query.rs); `privmsg.c` PRIVMSG/NOTICE/TAGMSG and broadcasts (privmsg.rs); `account_cmds.c` NICK/SETNAME/AUTHENTICATE (account.rs) |
| ~~`src/net/conn.c`~~ | 938 | **Done** (conn.c 345, conn_read.c 267, conn_write.c 338). `conn.c` refcount, close/fail, setup timeout, views; `conn_write.c` write queue, rate timer, `flush_writes`, shutdown, `conn_format_line`; `conn_read.c` `conn_parse_line`, `process_lines`, TLS input, `on_read` |
| ~~`src/user/user.c`~~ | 828 | **Done** (moved only): user.c 476, clients.c 357; `attach_run` (101 lines) still to split. `attach.c`: `attach_run` (101 lines, split into network selection, view creation and replay steps), `client_snapshot`, `register_done`; `user.c`: lifecycle, `start_run`, stop and event queue |
| `src/db/hrana.c` | 808 | `hrana.c` protocol and value encoding; `hrana_pipeline.c` request pipelining (`pipeline` 124 lines) |
| ~~`src/config/config.c`~~ | 741 | **Done**: config.c 237, config_directives.c 474 (8 handlers in a table, longest 65 lines), config_util.c 137. `parse_into` (337 lines) becomes table-driven, `{name, handler}` per block kind; handlers go in `config_directives.c` grouped by area; `config.c` keeps defaults, load/free, duration and float parsing. Directive order and error text stay identical, which the oracle `config`/`scfg` ops check |
| `src/db/turso.c` | 663 | By role: connection and routing (`turso.c`), statement execution (`turso_exec.c`) |
| ~~`src/upstream/upstream.c`~~ | 640 | **Done**: receiving moved to `upstream/receive.c`, `upstream_receive_take` split into batch, error and early-message helpers. `upstream_receive_take` (152 lines) becomes a command-to-handler table, batches move to `batches.c`; `upstream.c` keeps lifecycle, `tick` and send |
| `src/db/messages.c` | 629 | `messages_search.c`: FTS query quoting and search; `messages.c`: storage, history (`db_list_message_rows` 123 lines), ID high water |
| `src/user/network.c` | 591 | `channel_state.c`: store/attach/detach/delete channel; `network.c`: network store, attrs, receipts, away/monitor |
| `src/db/tenant.c` | 585 | `tenant_migrate.c`: `rebuild_messages` (108 lines) and schema upgrades; `tenant.c` the rest |
| ~~`src/upstream/channels.c`~~ | 543 | **Done**: per-command handlers in a table, user state in `upstream/users.c`. `upstream_handle_channel` (376 lines, complexity 137) becomes a table; user state (NICK/QUIT/AWAY/ACCOUNT/CHGHOST/WHO cache) moves to `users.c`, as in soju-rs `upstream/users.rs` |
| `src/db/conn.c` | 538 | Statement cache and helpers out of connection setup |
| `src/service/user.c` | 525 | `user_change.c`: `apply_change`, `store_user`; `user.c`: create, delete, status, run |
| `src/db/users.c` | 521 | Device certificates to `device_certs.c` |
| `src/http/client.c` | 509 | Response parsing out of request sending |
| `tests/unit/test_db_sqlite.c` | 1323 | Split by query family to match `src/db/`; the shared fixture goes to `tests/support/db_fixture.c`; `tests/PORTED_TESTS.md` lists the new file of each test |
| `tests/integration/test_server.c` | 1265 | Split by scenario area: registration/SASL, networks/BouncerServ, history/CHATHISTORY and multiplex, on top of the existing `server_fixture.c` |
| `tests/unit/test_db_turso.c` | 1201 | Split like `test_db_sqlite.c`, sharing the query-family files' cases where they're the same |
| `tests/support/turso_mock.c` | 739 | Hrana protocol encoding out of the mock server |
| `fuzz/oracle_ops.c` | 539 | `oracle_ops.c` op table and C-side ops; `oracle_proc.c` running and talking to the Rust oracle process |

Close to the limit: `irc/message.c` 492, `net/tls.c` 487, `db/writer.c`
475, `user/upstream.c` 467 (`user_upstream_message` 157 lines),
`msgstore/msgstore.c` 457.

Longest `src/` functions besides those above (all to split, or justify as
flat dispatch): `user_history` 160, `db_store_network` 133,
`user_read_marker` 122, `upstream_handle_registration` 121,
`user_clientcert` 110, `write_batch` 105, `store_changes` 105,
`flag_set_parse` 104. `tools/metrics.sh` lists all 36.

### 3.2 How each split is done

1. Commit 1: move code verbatim into the new files (`git diff --color-moved`
   shows only moves), add `internal.h`, adjust includes and `static`.
   Only functions used across the new files lose `static`; they're declared
   in `internal.h`, never in the public header.
2. Commit 2 (optional): break up long functions inside the new files.
3. Update the module's header comment (which Rust file each `.c` mirrors),
   PLAN.md section 4 and `tests/PORTED_TESTS.md` if test names move.
4. Check: CI presets, fuzz seed replay, oracle, and a 10-minute run of the
   matching session fuzzer (`fuzz_downstream_session` for `downstream/`,
   `fuzz_upstream_session` for `upstream/`).

Order: `downstream.c` → `user/commands.c` → `upstream/channels.c` +
`upstream.c` → `conn.c` → `config.c` → `user.c` + `user/network.c` → `db/`
files → `service/user.c` → `http/client.c` → test and fuzz files → lint
gate. Protocol-facing files first: they changed most in phase 11 and the
differential suite checks them.

---

## 4. Phase C — Audit by dimension

Each dimension below lists what to look for, how to check it, and the
modules to start with (highest risk first). Run it module by module after
that module's split has landed, so findings point at the final file layout.

### 4.1 Security (attacker-controlled input)

Priority: everything reachable before authentication, then authenticated
clients, then upstream servers (also untrusted).

| Surface | Modules | Checks |
|---------|---------|--------|
| Pre-auth bytes | `net/proxyproto.c`, `net/tls.c`, `net/conn.c` (line reader), `net/identd.c`, `http/forwarded.c` | Bounds on every length field (PROXY v2 TLVs, line limit 32 KiB), integer overflow in size math (`ckd_*` used), behaviour on partial reads and on a client that sends nothing (timeouts), TLS ALPN rejection, SNI/cert handling |
| Registration and SASL | `downstream/registration`, `downstream/sasl.c`, `service/auth.c`, `auth/auth.c` | Base64 decode bounds, PLAIN authzid/authcid splitting with embedded NULs, AUTHENTICATE chunking (400-byte rule, `+`), state machine can't skip auth (fuzz with commands in every order), password and SASL buffers wiped (`explicit_bzero`) after use, constant-time comparisons where secrets are compared, bcrypt cost/timing not leaking whether a user exists |
| Authenticated client | `downstream/*`, `user/commands.c`, `service/*` | Admin-only commands checked on every path (including deferred commands), one user can't name another user's network/channel IDs, BouncerServ argument parsing, CHATHISTORY limits and bounds, METADATA key/value limits, MONITOR list limits |
| Upstream server | `upstream/*` | Oversized/malformed numerics, ISUPPORT values (CHANMODES, PREFIX, CASEMAPPING, TARGMAX) with hostile contents, nick/channel names that collide after casemapping, batch nesting depth, labels that don't belong to us |
| Database | `db/*` | All SQL uses bound parameters (grep for `printf`-built SQL), FTS query quoting (`quote_fts_query`), file permissions of `soju.db` and the admin socket (0600/0775) |
| Process | `config`, `listener`, `certfp` | Unix socket path length, symlink handling when creating sockets, key file permissions on `certfp generate` |

Existing harnesses: `fuzz_irc_line`, `fuzz_downstream_session`,
`fuzz_upstream_session`, `fuzz_http_request`, `fuzz_ws_frame`,
`diff_oracle`. Tools: new fuzz harnesses `fuzz_proxyproto`, `fuzz_identd`,
`fuzz_forwarded`, `fuzz_sasl` (PLAIN/EXTERNAL/OAUTHBEARER payloads),
`fuzz_isupport`; add `proxyproto`/`forwarded` ops to the oracle so they're
checked against soju-rs too. Each harness asserts a property (round trip,
bounds), not just "no crash".

### 4.2 Memory safety and leaks

The rules of PLAN.md 2.1 are the checklist. For each module:

- Every `x*alloc` has one owner; ownership-taking functions (`*_take`) are
  named and documented as such; every error path between allocation and
  hand-off frees (`goto out` or `AUTOFREE`).
- Every asynchronous completion (DB job, CPU job, uv callback, timer) looks
  its object up by ID (rule 2) rather than holding a raw pointer across the
  wait. Grep for pointers stored in job structs.
- `uv_close` callbacks: no handle freed before its close callback; no
  callback touching a freed parent (`timer_closed`, `finish_shutdown`).
- Library-owned memory freed with the library's function (utf8proc,
  yyjson, OpenSSL `*_free`).
- `REF_TRACK` on every long-lived object; tests fail on live objects at
  exit (PLAN.md 2.4).

Checks: the full preset matrix including `clang-msan` and `*-valgrind`;
**L3 disconnect-at-every-byte** extended to SASL and multiplex attach/detach;
**allocation-failure injection is not needed** (xmalloc aborts), but
**SQLite fault injection** (`test_db_faults.c`) is extended to every query
family; LSan suppressions reviewed — each entry must link an upstream bug.

### 4.3 Correctness and edge cases

Reference: soju-rs behaviour. Per module, list the edge cases and confirm a
test exists for each:

- **IRC parsing/formatting** (`irc/message.c`, `xirc.c`, `ircutil.c`):
  empty params, trailing `:` alone, 15+ params, tag escaping
  (`\:`, `\s`, trailing `\`), duplicate tags, tags over 8191 bytes, NUL and
  invalid UTF-8 (lossy rules), CR in the middle of a line.
- **Casemapping** (`casemapping.c`): `rfc1459` vs `rfc1459-strict` vs
  `ascii` vs `precis`, mid-session CASEMAPPING change re-keying maps.
- **Time** (`timeutil.c`): 0–9 fractional digits, leap seconds, timestamps
  before 1970 and after 2038, clock skew rules (README "Differences").
- **Multiline/split** (`multiline.c`, `genmsg.c`): split at UTF-8
  boundaries, the `MAX_MESSAGE_LENGTH` TODO in `genmsg.c:96`, `multiline`
  limits from the upstream.
- **Message IDs and history** (`msgstore/*`, `db/messages.c`): IDs never
  reused across expiry, CHATHISTORY BEFORE/AFTER/AROUND/BETWEEN/LATEST with
  equal timestamps and limits 0/1/max, read markers moving backwards.
- **State machines**: downstream registration (busy/pending input),
  upstream connect/backoff/regain-nick, user start/stop/attach/detach.
  Draw each as a state table in its module header; the session fuzzers
  check no transition is missing (assert on unexpected states).
- **Open TODOs**: `irc/ircutil.c:199` (multi-prefix), `irc/whox.c:134`,
  `irc/genmsg.c:96`, `downstream.c:1398`, `downstream.c:1998`,
  `user/webpush.c:368` — check each still matches soju-rs and decide
  port-now or keep-as-upstream.

Tools: oracle differential runs for every pure function (extend
`oracle_ops` for anything not yet listed: `genmsg`, `multiline`, ISUPPORT
parsing); `tests/PORTED_TESTS.md` cross-checked against `#[test]`s on
`main` (any not yet ported get ported).

### 4.4 Reliability and robustness

- **Timeouts everywhere a peer can stall**: registration, TLS handshake,
  write timeout, PROXY header, identd, upstream registration (60 s),
  SASL with a slow authenticator. Test each with a peer that stops sending.
- **Back-pressure**: write queue growth with a client that never reads
  (bounded? disconnect policy matches soju-rs?), DB job queue under a burst,
  upstream command queues (64 per type).
- **Reconnect**: backoff with jitter bounds, no reconnect storm after a
  network outage, pinned-fingerprint failure doesn't loop fast.
- **Shutdown and reload**: SIGTERM with pending DB jobs, with buffered
  writes, mid-SASL; everything freed (PLAN.md 2.4). SIGHUP reload (phase 10) is
  in place: audit its races with live clients.
- **Database failures**: disk full, locked database, corrupt page — the
  bouncer logs and keeps serving instead of aborting where soju-rs does.
- **Threading**: only the DB thread touches SQLite; cross-loop events only
  through inboxes (rule 1); TSan on all integration tests with N > 1 loops.

Tool: an L7-lite soak (1 h, scripted clients connecting/disconnecting,
upstream flaps) under ASan with RSS and live-object count sampled every
minute; must be flat after warm-up.

### 4.5 Performance and optimization

Measure before changing anything; optimizations are S3 at most unless a
benchmark shows a cliff.

- Hot paths: line parsing/formatting, casemapped map lookups, message
  fan-out to N downstreams, history queries, write batching.
- Benchmarks: `tests/bench/` (not run in CI) with a fixed workload — 1k
  channels, 10k messages replayed, 100 downstreams — compared against
  soju-rs on the same machine.
- Look for: repeated `casemap` allocations per lookup, `strbuf` growth
  without reserve in loops, O(n²) list scans in channel/member handling,
  per-message SQLite statement preparation (cache prepared statements),
  missing indexes for history queries (`EXPLAIN QUERY PLAN`).
- `perf record` / `heaptrack` on the soak run to find allocation hot
  spots; mimalloc stats in debug.

### 4.6 Code quality

- Conventions doc followed: no banned functions (`tools/lint.sh` greps),
  `[[nodiscard]]` on every fallible function, `struct err **` pattern, naming
  of ownership (`_take`, `get_`, `peek_`).
- Every module header says which Rust file it mirrors and lists its
  deliberate differences.
- Dead code: functions with no callers (coverage report + `-Wunused`
  with LTO), unused struct fields.
- Duplicated helpers across `downstream/`, `user/` and `service/` (reply
  formatting, target parsing) consolidated into `irc/` or `user/util.c`.
- Promote the clang-tidy checks from phase A that turn out clean to errors
  in `.clang-tidy`, so they stay clean.

---

## 5. Order of work

| # | Work | Depends on | Size | Status |
|---|------|------------|------|--------|
| 1 | Phase A tooling + baseline | — | S | done; fuzzing deferred |
| 2 | Split `downstream.c` | 1 | M | done: 9 files, largest 424 lines |
| 3 | Split `user/commands.c`; audit of client commands | 1 | M | split done: 6 files, largest 320 lines; audit open |
| 4 | Split `upstream/channels.c` + `upstream.c`; security audit of upstream input | 1 | M | splits done (channels.c 333, users.c 280, upstream.c 454, receive.c 212); `upstream_receive_take` still has complexity 27; audit open |
| 5 | Security + memory audit of `net/*`, `downstream/*` (incl. new fuzzers) | 2 | L | A-04 (close re-entrancy) closed; rest open |
| 6 | Split `conn.c`; robustness audit of `net/` | 5 | M | split done; audit open |
| 7 | Split `config.c`; audit config + scfg | 1 | M | split done; audit open |
| 8 | Split `user.c`, `user/network.c`, the `db/` files, `service/user.c`, `http/client.c`; correctness audit of `user/`, `db/`, `msgstore/`, `service/` | 1 | L | |
| 9 | Split the test and fuzz files; add the 500-line check to `tools/lint.sh` | 8 | S | |
| 10 | Cross-cutting: threading/TSan, shutdown, soak | 2–9 | M | |
| 11 | Performance benchmarks and fixes | 10 | M | |
| 12 | Quality pass, promote lint rules, update PLAN.md section 4 | all | S | |

Steps 2–8 can be interleaved, one module at a time; each step ends with a
commit (or series) that passes CI and updates `doc/audit-findings.md`.

## 6. Done when

- No `.c`/`.h` file in `src/`, `tests/` or `fuzz/` over 500 lines, enforced
  by `tools/lint.sh`; no function over 80 lines except flat dispatch, and
  each one over 60 lines has a comment saying why it isn't split.
- Every S1/S2 finding fixed with a test; S3 fixed or recorded as a
  deliberate difference; S4 fixed or closed.
- New fuzzers (section 4.1) each run ≥ 1 CPU-hour clean; existing ones
  re-run clean after the refactor.
- Line coverage of `src/` measured and every uncovered function either
  tested or justified.
- 1-hour soak flat on RSS and live objects; full preset matrix green.
