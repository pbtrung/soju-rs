# Remaining work and verification limits

[Audit summary](../audit_results.md) · [Record index](README.md)

## 14. Remaining Issues and Blockers

**Remaining CI findings:** CI-001 automatic/test-before-release enforcement and CI-002 reproducibility remain open outside the requested implementation plan. REL-001 is fixed. No critical defect was confirmed.

**Remaining verification work:** STEP-024–025 now supply parser properties, bounded mutation corpora, migration/concurrency integrity stress and measured three-minute resource retention/cleanup. STEP-028–029 retain deployment/compatibility and final performance assessment. Coverage-guided continuous campaigns and multi-hour production soaks remain limits of the finite Phase 8 checks.

**Limits and further measurement:** memory-only receipt retention remains unchanged and outside the aggregate history/persistent-cache budgets. Blocking actor handoffs have a measured uncontended throughput cost. The documented finite counter/RSS, contention and replay tests do not establish whole-process memory limits, arbitrary-host latency bounds, production capacity or external transcript equivalence.

**Current verification limits:** cargo-audit/deny/machete are installed for the implementation run and their results are recorded in [verification](verification.md). STEP-030 removes chrono and the full cargo-deny policy passes without advisory ignores; duplicate-major warnings remain visible. cargo-outdated and coverage-guided fuzz tooling were not provisioned; the stable tests/fuzz mutation runner executed 80,000 cases; pinned Go/ergo and Rust baseline/final comparisons ran in Phase 7, while IRCv3/plumbing/gamja remain unrun. Phase 8 now records actual three-minute soaks; multi-hour soaks remain unrun. Raw Go transcript differences remain a compatibility limitation. A refreshed Cargo index now reports no compatible updates. No approval rejection prevented an action; registry/socket checks used approved escalation.

**Execution boundary:** the original audit deferred implementation under [audit_prompt.md §17.5](audit_prompt.md#175-execution-order--mandatory). The user subsequently authorized phases 1–8 and the STEP-030 migration. Earlier steps used commit/push after each step; Phases 7–8 allow compilation checks between steps and defer tests/lint/gates until all implementations in the phase are written. STEP-018–020 similarly preceded combined final verification. Native instrumentation and the CI-hardening phase are removed from the plan by user request; other later implementation phases remain outside the current request. Existing step/phase numbers are retained for stable references, so STEP-026–027 are retired rather than reassigned. Mark findings Completed only after implementation and verification.

## 16. Final Recommendations

Phases 1–8 now establish release preflight, current compiler/Clippy policy and warning-clean documentation, reviewed dependencies with the Jiff migration, bounded history/session/push work, a bounded lazy durable-receipt cache, owned writer/mention lifecycles, autonomous command deadlines, blocking actor execution and issuer-bound OAuth metadata, followed by typed history queries, cohesive command handlers and separated service/configuration responsibilities. The final Phase 6–8 gates, transcript limits and measured tradeoffs are recorded in [§12](verification.md#12-verification-matrix) and the resource/handler guides.

Phase 8 parser/mutation, migration/concurrency and finite soak steps are complete; final deployment/performance assessment remains pending. Raw Go comparisons expose existing feature/output and timing differences; completing Phase 7 does not close the full compatibility audit. Local quality gates remain required. CI automation findings stay open outside the implementation plan per the user's scope decision. Close the full audit only after the remaining in-scope compatibility and resource checks have actual recorded results.

The [record index](README.md) links the preserved audit and authoritative implementation checklist. Implementation outcomes are recorded by finding status and in [§15](progress.md#15-implementation-progress-log); baseline measurements describe the original audit.
