# Phase 11 work in progress

Qualification was stopped at the user's request on 2026-10-29. This is a
checkpoint, not a qualified release. The version is v0.99.0.

Completed before the checkpoint:

- Phase 10 process glue and llhttp integration are committed separately.
- All 165 mapped Rust unit tests and 17 integration scenarios are ported.
- GCC/Clang debug, ASan and TSan, plus Clang MSan passed before the latest
  protocol changes. The ASan suite subsequently passed 401 tests, before
  the final metadata batching and curl cleanup changes.
- Alpine GCC/Clang debug and release suites passed before those final changes.
- Ten-minute real-server soaks used two Ergo instances, 200 users and 2,000
  clients. C ran 604 seconds with 299 reconnects, peak RSS 160,556 KiB and
  zero measured RSS slope; Rust ran 601 seconds with 378 reconnects and
  peak RSS 591,312 KiB. Both shut down successfully. These separate runs
  do not establish a controlled throughput comparison.
- Focused three-way registration, registration-error and capability
  negotiation scenarios matched Rust after protocol corrections.

Unfinished checks:

- Full three-way differential, IRCv3, transport and gamja qualification.
  The last differential run still reported metadata differences; subsequent
  batching and blocked-source fixes have not been requalified.
- GNU Valgrind reported reachable OpenSSL/ngtcp2 allocations. Explicit
  curl cleanup before OpenSSL cleanup is included but not yet verified.
  Earlier Alpine Valgrind failures involved test deadlines; timeout scaling
  is included. No suppressions were added.
- Re-run the complete compiler/sanitizer/musl matrix and clang-tidy after
  all checkpoint changes. Final edits were interrupted without full checks.
- Complete release archives, controlled performance comparison and static
  analysis. The release script is WIP; no release has been published.

The requested soak budget is **10 minutes**, replacing the old 24-hour
release soak requirement. Longer fuzz budgets, heap attribution and other
unrun gates must not be reported as complete.
