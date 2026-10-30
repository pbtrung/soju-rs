# Audit findings

Tracked per [audit-plan.md](audit-plan.md) section 1, rule 5: one row per
finding. Severity S1–S4 as defined there.

| ID | Module | Dimension | Severity | Finding | Status | Commit | Test |
|----|--------|-----------|----------|---------|--------|--------|------|
| A-01 | net, config, server | concurrency | S4 | `concurrency-mt-unsafe`: `strerror` in code running on several loops (`net/conn.c:25`, `net/tls.c:38,61`, `net/listener.c:423`, `server/config.c`, `config/config.c`) | Closed: false positive. glibc ≥ 2.32 returns constant strings or a thread-local buffer, musl only constant strings | — | — |
| A-02 | net/loops, sojuctl | concurrency | S4 | `getenv` (`net/loops.c:18`, `main/sojuctl.c`) | Closed: called before any thread starts | — | — |
| A-03 | config/scfg, service, upstream, db/writer | quality | S4 | `misc-no-recursion` (23): nested config blocks, the BouncerServ help/command tree, multiline batch re-entry in `upstream_receive_take`, `write_chunk` bisection | Closed: each depth is bounded (config nesting by the file, the fixed command tree, one batch level, log2 of 100 rows). Keep the check off | — | — |
| A-04 | downstream, net/conn | quality | S3 | `misc-no-recursion`: `downstream_close` → `conn_close` → callbacks → `downstream_input_done` → `drain_input` → `process_input` → `downstream_close`, and `conn_unref` → `conn_fail` → `conn_close` | Open: re-entrancy through close callbacks; check in step 5 (net/downstream audit) that each path is guarded by a `closed` flag | — | — |
| A-05 | net/listener, db | quality | S4 | `bugprone-assignment-in-if-condition` (4): `db/hrana.c:451`, `db/users.c:112,408`, `net/listener.c:301` | Open: rewrite during the split of these files | — | — |
| A-06 | many | performance | S4 | `performance-no-int-to-ptr` (10): IDs passed as `void *` callback data (`(void *)(uintptr_t)id`) | Closed: deliberate, objects are looked up by ID (PLAN.md 2.1 rule 2) | — | — |
| A-07 | many | quality | S4 | `bugprone-easily-swappable-parameters` (72), `bugprone-multi-level-implicit-pointer-conversion` (60), `misc-include-cleaner` (3,932, mostly transitive includes) | Closed: keep these checks off, as `.clang-tidy` has them | — | — |
| A-08 | http/client, db/hrana, config, upstream, user | quality | S3 | `readability-function-cognitive-complexity` over 40 in 40 functions. Real ones: `parse_into` 349, `upstream_handle_channel` 292, `user_handle_command` 127, `user_upstream_message` 103. `http/client.c` `setup` (10,823) and `get_loop`, and `db/hrana.c` `http`, are inflated by curl's type-checking `curl_easy_setopt` macros | Open: split in phase B steps 3, 4 and 7; exclude the curl macro expansion before promoting the check | — | — |

## Baseline (phase A)

Measured on 2026-10-30 with `tools/coverage.sh` and `tools/metrics.sh`;
the summary is in audit-plan.md section 2.1. To regenerate:

```sh
tools/metrics.sh          # files over 500 lines, functions over 80 lines or CCN 15
tools/coverage.sh         # build/coverage/summary.txt and html/
```

## Phase A static analysis

`run-clang-tidy` with the checks `.clang-tidy` doesn't enable
(`performance-*`, `concurrency-*`, cognitive complexity over 40) and the
ones it disables, over `src/` with the clang-asan compile database. The
`clang-analyzer-*` checks already run on every commit (`.clang-tidy`) and
report nothing. Rows A-01 to A-08 are the triage. Regenerate with:

```sh
run-clang-tidy -quiet -p build/clang-asan -config='{Checks: "-*,performance-*,concurrency-*,readability-function-cognitive-complexity,bugprone-easily-swappable-parameters,bugprone-assignment-in-if-condition,bugprone-reserved-identifier,misc-no-recursion,misc-include-cleaner,cert-dcl37-c,bugprone-multi-level-implicit-pointer-conversion", CheckOptions: {readability-function-cognitive-complexity.Threshold: 40}, WarningsAsErrors: ""}' "$PWD/src/"
```
