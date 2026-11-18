# Original audit baseline and compiler assessment

[Audit summary](../audit_results.md) · [Record index](README.md)

This records the original audit context. Current status is in the
[summary](../audit_results.md), [findings](findings.md) and
[verification](verification.md). Original measurements and tool availability
are historical, with later outcomes identified explicitly.

## 1. Executive Summary

Documentation maintenance: the audit and its prompt now live in `doc/`.
Source locations below are relative to the repository root. Audit findings,
verification results describe the 2026-11-17 audit baseline. Finding and
checklist statuses track subsequent implementation; documentation cleanup alone
does not mark code fixes complete.
The README's release and database descriptions have since been corrected, and
older review records moved to `doc/archive/`. Implementation progress and
remaining work are recorded by finding status and in [§15](progress.md#15-implementation-progress-log).

Audit date: 2026-11-17. Reviewed commit: `c103b74be47920e307b99efe4ff739a1614ec7a1` (`soju` 0.99.25).

The original execution was **audit and planning only**, as required by [audit_prompt.md §17.5](audit_prompt.md#175-execution-order--mandatory), which superseded its earlier implementation instructions. No Rust source, manifests, lockfile, tests, release scripts, or workflows were changed. Finding statuses below track subsequent verified implementation. Build artifacts and temporary audit probes were generated; this report was that run's only new deliverable. The pre-existing untracked `audit_prompt.md` is preserved at `doc/audit/audit_prompt.md`.

The baseline is healthy: formatting, all-target checking, default Clippy with warnings denied, and 210 tests pass on GNU; the same 210 tests pass on musl. Edition 2024 is already in use. Rust 1.100.0 is installed and its official release is verified. Default documentation generation succeeds with 13 warnings; documentation with warnings denied fails. The requested expanded lint policy fails with thousands of diagnostics and requires staged remediation.

The original priorities were broken release dispatch, missing automatic/test-before-release gates, unlimited history target retention, and unlimited notification task admission. Two smaller lifecycle defects were reproduced without changing the repository: cancellation leaks the downstream metric increment, and dropping connection handles leaves the writer/stream alive. The dependency inventory found `rand` 0.10.4 and `rusqlite` 0.43.0 newer than the declared/locked versions. No critical vulnerability, reachable undefined behavior, authentication bypass, or database corruption was established by this audit.

Scope and assurance: repository-wide inventory, static searches, compiler/Clippy analysis, targeted source tracing across security/resource/persistence paths, existing tests, release builds, and authoritative dependency research. This is not an exhaustive proof of every branch, dependency's internals, memory safety, or long-duration stability. Potential risks and optional improvements are explicitly identified. The prior [archived review](../archive/code-audit.md) describes previous fixes; it is historical context, not evidence that this execution fixed anything.

## 2. Repository and Toolchain Baseline

| Item | Observed baseline |
| --- | --- |
| Package/workspace | Single package `soju` 0.99.25; library plus `soju` and `sojuctl` binaries |
| Rust source | 101 files under `src/`, 27,589 lines including embedded tests and benchmark |
| Edition | 2024 already; no edition migration needed |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09) (Arch Linux rust 1:1.100.0-1)` |
| Cargo | `cargo 1.100.0 (3acc79c0c 2026-11-05)` |
| MSRV/toolchain policy | No `rust-version`, toolchain file, or checked-in Cargo lint policy; README says latest stable |
| Toolchain management | Distro Rust; `rustup` unavailable. GNU/musl standard libraries and `musl-gcc` installed |
| Features | No package feature flags; 35 direct dependencies, Tokio also a dev dependency |
| Lock inventory | 270 packages including the local package; metadata contains platform-specific/optional graph entries |
| Native code | No application `unsafe` found. Bundled SQLite 3.54.1 via `libsqlite3-sys` 0.40.0, AWS-LC via `aws-lc-sys` 0.47.0, mimalloc via `libmimalloc-sys` 0.1.49 |
| Build scripts | No repository `build.rs`; native dependencies have their own build systems |
| Database | SQLite WAL, synchronous NORMAL, single connection behind a mutex; schema 35; migrations use an immediate transaction |
| Async architecture | User-owned mutable state, network runners, connection writers, input permits, timers and deferred tasks |
| CI | Two manually dispatched workflows; GNU/musl tests and release packaging exist separately |
| Original checkout | No tracked modifications; untracked `audit_prompt.md` (subsequently moved into `doc/`) |

Repository areas examined:

| Area | Files and review focus |
| --- | --- |
| Configuration/CLI | `Cargo.toml`, `Cargo.lock`, `config.in`, `src/config.rs`, `scfg.rs`, `flags.rs`, both binaries; runtime selection, directives, limits, startup/reload |
| Networking/security | `src/conn/`, `listen/`, `proxyproto.rs`, `tls.rs`, `auth.rs`, `certfp.rs`, `identd.rs`, `webpush.rs`; framing, trust boundaries, client authentication, pinning, size/time limits |
| IRC/state | `src/irc/`, `ircutil.rs`, `xirc/`, `downstream/`, `upstream/`, `user/`, `service/`; dispatch, authorization, SASL, batches, labels, reconnect, multiplexing, delivery |
| Persistence | `src/database/`, SQL schemas/indexes/triggers/migrations, `src/msgstore/`; transactions, scoped queries, ID allocation, retention, FTS, shutdown writes |
| Operations | `server/`, `logger.rs`, `metrics.rs`, `rate.rs`, `util.rs`; task ownership, lock use, CPU admission, shutdown, observability |
| Tests/tooling | Unit modules, `tests/server*`, `tests/listeners.rs`, all compatibility scripts, Playwright test/config, reader benchmark and Python benchmark |
| Delivery/docs | Both workflows, all release scripts, README and `doc/`; release ordering, compatibility claims, GNU/musl deployment and existing performance evidence |

Baseline commands and actual outcomes are in [§12](verification.md#12-verification-matrix). Socket tests initially failed under the sandbox (`Operation not permitted`); rerunning with approved local socket access passed. These environmental failures are not application defects.

## 4. Rust 1.100 and Edition 2024 Modernization

Original and audited compiler are the same: **1.100.0**. Original and proposed edition are **2024**. The [official stable release notes](https://doc.rust-lang.org/stable/releases.html) list Rust 1.100.0 released on 2026-11-12; the local compiler identifies the matching commit. This run therefore has no unavailable-1.100 limitation. No compiler installation or edition rewrite was performed.

The code already uses let chains, let-else, `Option::is_some_and`/`is_none_or`, `OnceLock`, checked message-ID allocation and saturating backoff. Retain those readable idioms. Prefer `TryFrom`, pattern matching and borrowing validated state over index/unwrap pairs. Use a small RAII guard for metric/resource cleanup; evaluate the new standard drop-guard facilities only if they simplify the actual ownership contract. Do not introduce custom allocators, never-type rewrites or new syntax merely because 1.100 provides them. No deprecated API warning was observed in the normal compiler/Clippy baseline.

Proposed policy: explicitly document the support floor, test stable/MSRV, and leave production stable-only. Read the exact target release notes before selecting APIs. Reference: [Edition 2024 guide](https://doc.rust-lang.org/edition-guide/rust-2024/index.html).

## 6. Compiler and Clippy Hardening

Default command passes:

```sh
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The following diagnostic trial actually ran and **failed**:

```sh
cargo clippy --workspace --all-targets --all-features --locked -- \
  -D warnings -W clippy::pedantic -W clippy::unwrap_used \
  -W clippy::expect_used -W clippy::panic -W clippy::indexing_slicing \
  -W clippy::string_slice -W clippy::cast_possible_truncation \
  -W clippy::cast_possible_wrap -W clippy::cast_sign_loss \
  -W clippy::let_underscore_must_use -W clippy::let_underscore_future \
  -W clippy::map_err_ignore -W clippy::allow_attributes
```

Compiler summaries: 1,773 lib diagnostics and 2,453 lib-test diagnostics, with overlapping code. Text-log link occurrences include 924 indexing, 685 unwrap, 142 must-use candidate, 121 missing-errors-doc, 81 doc-Markdown, 38 ignored must-use, 33 string-slice and 23 too-many-lines diagnostics. These are diagnostic occurrences, not unique defects, and other targets can reveal further issues after the library is remediated.

Apply the prompt's rust lint baseline (`unsafe_code = forbid`; deny unused must-use/imports/variables, unreachable code and unfulfilled expectations) and Clippy all/pedantic with negative group priorities, then its selected restriction lints. Explicitly include todo/unimplemented/dbg prohibitions even though the trial focused on observed categories. Audit each ignored send result: a receiver intentionally gone at shutdown differs from lost mandatory persistence. Test fixture unwraps can receive narrowly scoped, explained exceptions, or propagate fixture setup errors; do not weaken the entire production crate for tests. Guarded byte indexing is not automatically a bug; prefer `get`, slice patterns and `split_first` where they communicate the check cleanly.

Use the [Clippy lint reference for Rust 1.100](https://rust-lang.github.io/rust-clippy/rust-1.100.0/index.html) and local compiler support. No blanket restriction group or unexplained suppression is proposed.
