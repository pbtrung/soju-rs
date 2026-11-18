# Remaining work and verification limits

[Audit summary](../audit_results.md) · [Record index](README.md)

## 14. Remaining Issues and Blockers

**Remaining CI findings:** CI-001 automatic/test-before-release enforcement and CI-002 reproducibility remain open outside the requested implementation plan. REL-001 is fixed. No critical defect was confirmed.

**Verification complete within scope:** all nine phases and 28 sequential steps are verified. STEP-027–028 add archive/restored-database deployment, IRCv3/plumbing, local Gamja/Senpai, original/final Rust comparisons and performance/resource measurements. See [final assessment](final-verification.md) for actual results and limitations. Coverage-guided continuous campaigns and multi-hour production soaks remain outside these finite checks.

**Limits and further measurement:** memory-only receipt retention remains unchanged and outside the aggregate history/persistent-cache budgets. Blocking actor handoffs have a measured uncontended throughput cost. The documented finite counter/RSS, contention and replay tests do not establish whole-process memory limits, arbitrary-host latency bounds, production capacity or external transcript equivalence.

**Current verification limits:** earlier cargo-audit/deny/machete results remain recorded in [verification](verification.md); the tools are absent in the final session, so advisory freshness was not re-established. STEP-008 removed chrono and earlier full cargo-deny checks passed without advisory ignores. cargo-outdated and coverage-guided fuzz tooling were not provisioned. Final mutation campaigns reran 80,000 cases. IRCv3 32/32, plumbing 20/20, local Gamja 4/4 against each bouncer and Senpai real-bouncer checks pass. Raw Go transcript differences remain; the original/final Rust scenarios pass with service-completion fixture corrections. Final three-minute soaks pass; multi-hour production soaks and full deployment coverage remain unestablished. Registry/socket sandbox failures were rerun with approved access.

**Execution boundary:** the original audit deferred implementation under [audit_prompt.md §17.5](audit_prompt.md#175-execution-order--mandatory). The user subsequently authorized phases 1–9 and the STEP-008 migration. Earlier steps used commit/push after each step; Phases 7–8 allow compilation checks between steps and defer tests/lint/gates until all implementations in the phase are written. STEP-019–021 similarly preceded combined final verification. Native instrumentation is excluded; CI findings remain open outside the implementation scope. Final verification is Phase 9; step IDs follow checklist order. Mark findings Completed only after implementation and verification.

## 16. Final Recommendations

Phases 1–8 now establish release preflight, current compiler/Clippy policy and warning-clean documentation, reviewed dependencies with the Jiff migration, bounded history/session/push work, a bounded lazy durable-receipt cache, owned writer/mention lifecycles, autonomous command deadlines, blocking actor execution and issuer-bound OAuth metadata, followed by typed history queries, cohesive command handlers and separated service/configuration responsibilities. The final Phase 6–8 gates, transcript limits and measured tradeoffs are recorded in [§12](verification.md#12-verification-matrix) and the resource/handler guides.

Phase 9 completes the planned verification and performance assessment with the limits in the [final record](final-verification.md). Raw Go comparisons retain feature/output and timing differences even though the tested client forks pass. Local quality gates remain required for future changes. CI automation findings stay open outside the implementation scope; no claim of exact Go equivalence or complete production coverage follows from checklist completion.

The [record index](README.md) links the preserved audit and authoritative implementation checklist. Implementation outcomes are recorded by finding status and in [§15](progress.md#15-implementation-progress-log); baseline measurements describe the original audit.
