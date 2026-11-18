# Dependency maintenance

Reviewed on 2026-11-17 with Rust 1.100.0. The package uses 35 direct
dependencies; the reduced lockfile contains 266 packages including soju.
The original inventory remains in [the audit report](dependency-assessment.md#5-dependency-upgrade-assessment).

## Completed updates

| Dependency | Previous | Current |
| --- | --- | --- |
| rand | 0.10.3 | 0.10.4 |
| rusqlite | 0.42.0 | 0.43.0 |
| chrono → jiff | chrono 0.4.45 | jiff 1.0.0 |
| bumpalo | 3.20.3 | 3.20.4 |
| cc | 1.11.1 | 1.11.2 |
| errno | 0.3.14 | 0.3.15 |
| mio | 1.2.4 | 1.2.5 |
| rustls-webpki | 0.103.16 | 0.103.17 |
| zeroize | 1.9.1 | 1.9.2 |

The [rusqlite release](https://github.com/rusqlite/rusqlite/releases/tag/v0.43.0)
changes empty/comment-only statement handling and fixes optional APIs. Our
queries require no API adaptation. Bundled libsqlite3-sys remains 0.40.0
(SQLite 3.54.1); schema 35 and persisted message IDs are unchanged. The new
[file-backed regression](../../src/database/sqlite/tests/migrations.rs) checks
migration/reopen behavior, FTS, foreign keys, delivery cursors and ID allocation.

## Selected features

Tokio explicitly enables filesystem, buffered I/O, macros, networking, runtime,
signals, synchronization and timers. Unused process/stdio support is removed.
The existing parking_lot locking backend is retained; the dev dependency adds
only test-util to the production feature set. Tokio-util explicitly enables `rt`
for owned maintenance task tracking in STEP-019.

rusqlite explicitly enables bundled native SQLite, statement caching and scalar
functions. STEP-014 uses connection-local scalar functions for bounded receipt
queries with the same IRC case mapping and legacy lossy UTF-8 behavior as Rust.
No function-dependent index or schema migration is installed.
Disabling its default WASM backend removes sqlite-wasm-rs and rsqlite-vfs
from the lockfile. Required GNU and musl targets retain their native backend;
this selection does not promise WASM support. Existing reqwest and rcgen
choices already disable unnecessary defaults and are retained.

cargo-machete found no unused direct dependencies; none were removed based
only on a textual search. Parent-owned major-version constraints are retained
rather than forced through incompatible patches.

## Advisory and license policy

[deny.toml](../../deny.toml) rejects yanked crates and unreviewed Git/registry
sources. Its license list covers this AGPL project and the SPDX choices
present in the inspected dependency graph, including build/dev dependencies.
Duplicate major versions remain visible warnings; they are not suppressed.
No advisory IDs are ignored.

**Resolved:** [DEPS-002 / STEP-030](findings.md#deps-002--replace-unmaintained-chrono-with-jiff)
replaces chrono with [Jiff 1.0.0](https://docs.rs/jiff/1.0.0/jiff/), the current
stable release reviewed on 2026-11-17. chrono and its timezone backend are absent
from the lockfile. A fresh RustSec scan reports zero known vulnerabilities and
no advisory warnings. The full cargo-deny check passes without advisory ignores;
its existing duplicate-major warnings for syn and untrusted remain visible.

Checks used cargo-audit 0.23.0, cargo-deny 0.21.0 and cargo-machete 0.10.1.
Tools were installed into a temporary directory for this run; they are not
repository dependencies. With those tools installed on PATH, repeat:

```sh
cargo update --dry-run
cargo tree --locked --target all -e features
cargo audit --json --file Cargo.lock
cargo deny --all-features --locked check
cargo machete --with-metadata
```

A successful cargo-audit exit code does not mean that its warning list is
empty; inspect the JSON warnings as well as vulnerabilities.

## Timestamp compatibility

Public Rust timestamp fields and methods now use `jiff::Timestamp` in place of
`chrono::DateTime<Utc>`. Callers must update imports and constructors; this is a
Rust API change. Fixed elapsed intervals use `jiff::SignedDuration`, retaining
24-hour expiry intervals across DST. Configured maintenance durations use checked
subtraction and return errors when their result cannot be represented.
The separate `time` dependency remains necessary for rcgen certificate dates.

Server-time input still requires uppercase `Z`, accepts omitted fractions or
one through nine ASCII fractional digits, and retains nanoseconds in memory.
Output still truncates to exactly three fractional digits. Existing padding
leniency, pre-epoch values, leap-year rules, chronological comparisons, future
clock/read-marker handling, floor rounding of pre-epoch Unix topic seconds,
and canonical SQLite TEXT values are covered by regressions. Schema 35, IDs, FTS and delivery cursors are unchanged; opening a
database does not rewrite timestamp strings.

Two differences from the captured chrono fixtures are intentional:

- Jiff has no leap-second representation. Input second `60` is constrained to
  `59`, retaining its fraction, rather than preserving chrono's leap-second
  value. Existing leap-second TEXT remains readable and becomes second `59`
  if written back through the timestamp API.
- Jiff's timestamp range is narrower. The maximum UTC instant is
  `9999-12-30T22:00:00.999999999Z`; later four-digit dates that chrono accepted
  are rejected. Four-digit year `0000` remains supported. Unix topic seconds
  outside Jiff's range become an unknown topic time, as with out-of-range
  chrono values previously. See [Jiff's timestamp documentation](https://docs.rs/jiff/1.0.0/jiff/struct.Timestamp.html).

Local logs and configuration warnings retain `YYYY/MM/DD HH:MM:SS`, using
Jiff's system timezone and `TZ` selection. Isolated subprocess fixtures verify
New York spring/fall DST transitions and Tokyo output without changing the
parent test process's environment. Default Jiff timezone features are retained
for system zoneinfo and platform fallback support.

## Remaining parent-constrained versions

A refreshed registry dry run finds no remaining compatible updates. The
following locked versions differ from the latest stable releases in the
live crates.io inventory. The parent requirements explain why ordinary
resolution cannot advance them. Some parents are optional or platform-specific;
GNU/musl verification does not establish support for other targets. Revisit
these rows when parents publish compatible updates.

| Package | Locked | Latest stable | Parent requirements |
| --- | --- | --- | --- |
| asn1-rs-derive | 0.6.0 | 0.7.0 | asn1-rs 0.7.2 (^0.6) |
| asn1-rs-impl | 0.2.0 | 0.3.0 | asn1-rs 0.7.2 (^0.2) |
| bit-vec | 0.9.1 | 0.10.2 | yasna 0.6.0 (^0.9.1) |
| getrandom | 0.2.17 | 0.4.3 | ring 0.17.14 (^0.2.10) |
| lock_api | 0.4.14 | 0.5.0 | parking_lot 0.12.5 (^0.4.14) |
| nom | 7.1.3 | 8.0.0 | asn1-rs 0.7.2 (^7.0); der-parser 10.0.0 (^7.0); rusticata-macros 4.1.0 (^7.0); x509-parser 0.18.1 (^7.0) |
| num-bigint | 0.4.8 | 0.5.1 | der-parser 10.0.0 (^0.4) |
| parking_lot | 0.12.5 | 0.13.0 | tokio 1.54.1 (^0.12.0) |
| parking_lot_core | 0.9.12 | 0.10.0 | parking_lot 0.12.5 (^0.9.12) |
| quinn-udp | 0.5.16 | 0.6.3 | quinn 0.11.12 (^0.5) |
| r-efi | 6.0.0 | 7.2.0 | getrandom 0.4.3 (^6) |
| redox_syscall | 0.5.18 | 0.9.5 | parking_lot_core 0.9.12 (^0.5) |
| rusticata-macros | 4.1.0 | 5.0.0 | asn1-rs 0.7.2 (^4.0); der-parser 10.0.0 (^4.0); x509-parser 0.18.1 (^4.0) |
| syn | 2.0.119 | 3.0.10 | asn1-rs-derive 0.6.0 (^2.0); asn1-rs-impl 0.2.0 (^2.0); jni-macros 0.22.4 (^2.0); jni-sys-macros 0.4.1 (^2); synstructure 0.13.2 (^2) |
| synstructure | 0.13.2 | 0.14.0 | asn1-rs-derive 0.6.0 (^0.13) |
| tower-http | 0.6.11 | 0.7.1 | reqwest 0.13.5 (^0.6.8) |
| untrusted | 0.7.1 | 0.9.0 | aws-lc-rs 1.19.0 (^0.7.1) |
| wasi | 0.11.1+wasi-snapshot-preview1 | 0.14.7+wasi-0.2.4 | getrandom 0.2.17 (^0.11); mio 1.2.5 (^0.11.0) |
| windows-link | 0.2.1 | 0.100.0 | gethostname 1.1.0 (^0.2.1); jiff 1.0.0 (^0.2.1); jni 0.22.4 (^0.2); parking_lot_core 0.9.12 (^0.2.0); windows-sys 0.61.2 (^0.2.1) |
| windows-sys | 0.52.0 | 0.61.2 | ring 0.17.14 (^0.52) |
| windows-targets | 0.52.6 | 0.53.5 | windows-sys 0.52.0 (^0.52.0) |
| windows_aarch64_gnullvm | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_aarch64_msvc | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_i686_gnu | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_i686_gnullvm | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_i686_msvc | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_x86_64_gnu | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_x86_64_gnullvm | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |
| windows_x86_64_msvc | 0.52.6 | 0.53.1 | windows-targets 0.52.6 (^0.52.6) |

[Documentation index](../README.md)
