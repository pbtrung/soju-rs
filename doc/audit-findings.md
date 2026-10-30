# Audit findings

Tracked per [audit-plan.md](audit-plan.md) section 1, rule 5: one row per
finding. Severity S1–S4 as defined there.

| ID | Module | Dimension | Severity | Finding | Status | Commit | Test |
|----|--------|-----------|----------|---------|--------|--------|------|

## Baseline (phase A)

Measured on 2026-10-30 with `tools/coverage.sh` and `tools/metrics.sh`;
the summary is in audit-plan.md section 2.1. To regenerate:

```sh
tools/metrics.sh          # files over 500 lines, functions over 80 lines or CCN 15
tools/coverage.sh         # build/coverage/summary.txt and html/
```
