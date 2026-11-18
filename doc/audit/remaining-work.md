# Remaining work and verification limits

[Audit summary](../audit_results.md) · [Record index](README.md)

## 14. Remaining Issues and Blockers

All nine phases and 28 sequential steps are verified within the recorded scope.
Checklist completion does not establish exact Go equivalence or complete
production coverage. The [final assessment](final-verification.md) records the
actual deployment, client, performance and resource checks.

### Open findings

CI-001 (automatic checks and test-before-release enforcement) and CI-002
(reproducible test inputs/toolchains) remain open outside implementation scope.
REL-001's release-dispatch/preflight defect is fixed. No critical defect was
confirmed by the audit; that is not proof that none exists.

### Compatibility and dependency evidence

- IRCv3 32/32, plumbing 20/20, local Gamja 4/4 against each bouncer and Senpai's
  real-bouncer integration pass at the recorded revisions.
- All twelve selected Go/Rust transcript comparisons retain differences.
  Original/final Rust scenarios pass with service-completion fixture corrections.
- The Jiff migration removed chrono; earlier cargo-audit/deny/machete checks
  passed without advisory ignores. Those tools were absent in the final session,
  so advisory freshness was not re-established. cargo-outdated was not provisioned.
- Fixed-seed mutation campaigns reran 80,000 cases. Continuous coverage-guided
  fuzzing and multi-hour production soaks were not established.

### Resource and performance limits

Memory-only receipt/client-name retention remains outside the aggregate history
and persistent receipt-cache budgets. Actor handoffs have measured uncontended
throughput cost. Finite RSS/counter, contention and replay measurements do not
establish a whole-process memory limit, arbitrary-host latency, production
capacity or public-certificate/reverse-proxy deployment coverage.

### Execution boundary

The [source prompt](audit_prompt.md#175-execution-order--mandatory) originally
requested assessment only. Subsequent authorization covered phases 1–9 and the
Jiff migration. Phases 7–8 used compilation checks between implementations and
combined tests/lint/gates after each phase. Native instrumentation remained
excluded; CI findings stayed assessment-only. These are historical execution
constraints, not instructions to rerun a full audit for an ordinary change.

## 16. Final Recommendations

Preserve the verified ownership, protocol and storage contracts when extending
the implementation. Run the applicable [development checks](../development.md)
for future changes and choose benchmarks that exercise the affected path.
Keep failed comparisons and untested deployment cases explicit in test reports.

Use [findings](findings.md) for verified fixes and open issues,
[verification](verification.md) for command outcomes, and
[progress](progress.md) for the implementation timeline. The original baseline
is historical evidence, not a substitute for testing a changed build.
