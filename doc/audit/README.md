# Audit records

Start with the [audit summary](../audit_results.md). The original audit was on
2026-11-17; verified implementation and later record updates are dated separately.

| Record | Responsibility |
| --- | --- |
| [Implementation checklist](implementation-plan.md) | Authoritative step IDs, dependencies, acceptance and completion |
| [Findings](findings.md) | Severity, impact, fixes and verified finding status |
| [Verification](verification.md) | Actual PASS/FAIL/NOT RUN evidence, including failed comparisons |
| [Progress](progress.md) | Dated implementation outcomes and deviations |
| [Remaining work](remaining-work.md) | Open final verification, compatibility and measurement limits |
| [Original baseline](baseline.md) | Audit context, toolchain and compiler assessment |
| [Dependency assessment](dependency-assessment.md) | Original version/feature inventory and upgrade decisions |
| [Design review](design-review.md) | Resource, concurrency, architecture and compatibility analysis |
| [Source prompt](audit_prompt.md) | Audit-only workflow and current reporting/scope rules |

Supporting guides from this audit run:

- [Dependency maintenance](dependencies.md), including Jiff migration compatibility.
- [Resource limits](resource-limits.md), including actor-boundary measurements.
- [Protocol handlers](protocol-handlers.md), including typed service scope.
- [Parser/resource stress checks](resource-soaks.md), including Phase 8 reproduction and results.

Phases 1–8 and STEP-030 are verified. STEP-028–029 in Phase 10 remain pending.
The user excluded CI-hardening implementation; its findings stay open. The plan
retains original identifiers, with STEP-026–027 retired and no active CI phase.
Local test/lint gates remain required. Raw Go compatibility differences and
unrun browser/deployment/multi-hour checks remain visible in verification.
