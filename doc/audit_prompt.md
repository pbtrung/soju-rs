# Comprehensive Rust Code Audit, Modernization, Correctness, and Memory Safety Hardening

**Repository:** https://github.com/pbtrung/soju-rs

**Execution agent:** OpenAI Codex

**Target Rust version:** Rust 1.100 (stable, if officially available)

**Scope:** Entire repository, including source code, dependencies, build scripts, tests, CI, and documentation.

## 1. Mission

Act as a principal Rust engineer, security auditor, software architect, and code quality reviewer.

Perform a comprehensive, production-grade audit and modernization of the entire `soju-rs` repository.

This is an **implementation task, not merely a review**. Inspect the actual codebase, identify problems, implement justified improvements, add tests, and verify the results.

The objectives are:

1. Enforce the strictest practical Rust compiler and Clippy correctness checks.
2. Eliminate unsafe shortcuts, unchecked assumptions, and silent failures.
3. Identify and fix memory, resource-management, concurrency, and asynchronous programming bugs.
4. Modernize the codebase using stable Rust language and standard-library features available through Rust 1.100.
5. Upgrade all direct and transitive dependencies to the latest compatible stable releases, including major versions.
6. Improve architecture, maintainability, modularity, and readability.
7. Refactor oversized files, overly long functions, duplicated logic, and unnecessarily complex implementations.
8. Strengthen testing, fuzzing, static analysis, and CI.
9. Preserve existing behavior, protocol compatibility, database integrity, security, and performance.
10. Follow established Rust ecosystem conventions and sound software engineering principles.

Do not optimize for the smallest diff at the expense of long-term code quality. However, avoid unnecessary abstractions, speculative rewrites, and cosmetic churn.

**Primary principle: Correctness first, safety second, maintainability third, performance preservation throughout.**

All improvements must be justified, testable, and reviewable.

---

## 2. Repository-Wide Audit

Before editing, thoroughly inspect:

- Workspace structure and Cargo manifests.
- Rust edition and minimum supported Rust version.
- Rust source files and module boundaries.
- Binary and library targets.
- Feature flags and conditional compilation.
- Build scripts and native dependencies.
- Existing unsafe code and FFI boundaries.
- Async runtime, task management, and shared state.
- Networking, TLS, WebSocket, and IRC parsing.
- Authentication and authorization.
- SQLite storage, transactions, and migrations.
- Error handling and logging.
- Unit, integration, and compatibility tests.
- GitHub Actions and release workflows.
- GNU and musl target support.
- Existing performance-sensitive paths.

Establish a baseline using the existing toolchain and tests.

Produce an internal audit inventory identifying:

- Critical correctness or security bugs.
- Potential memory and resource leaks.
- Panic-prone code.
- Concurrency and cancellation hazards.
- Outdated dependencies.
- Deprecated APIs.
- Large or poorly structured modules.
- Duplicated implementations.
- Insufficient test coverage.
- CI enforcement gaps.

Prioritize issues according to severity and practical impact.

Do not make unsupported claims about bugs or vulnerabilities.

---

## 3. Rust 1.100 Modernization

Modernize the repository to use appropriate stable Rust features available through version 1.100.

### 3.1 Toolchain and edition

- Verify that Rust 1.100 has been officially released and is available.
- Use official Rust release notes and documentation to identify stable features.
- If Rust 1.100 is unavailable, use the latest available stable version and explicitly report the limitation.
- Evaluate upgrading the project to Rust Edition 2024.
- Use `cargo fix --edition` where appropriate.
- Review all edition migration warnings.
- Update `rust-version` and toolchain configuration consistently.
- Preserve required deployment targets.
- Do not introduce nightly-only features into production code.

### 3.2 Language and standard-library modernization

Review the codebase for opportunities to adopt modern stable Rust constructs, including:

- Improved pattern matching and let-chain expressions.
- Modern `if let` and `while let` patterns.
- Modern `Option` and `Result` combinators.
- Improved iterator APIs.
- Modern collection and slice APIs.
- Safer numeric conversions.
- Checked arithmetic.
- Improved error propagation.
- Modern lifetime and borrowing patterns.
- More expressive type-system features.
- Improved standard-library synchronization primitives.
- New stable APIs that can replace custom utilities or unnecessary dependencies.
- Improved const capabilities where useful.
- Modern async and future-related language features.

Evaluate each modernization against readability, correctness, portability, and performance.

Prefer standard-library functionality when it is mature, clear, and suitable.

Do not rewrite working code solely to use a newer syntax.

### 3.3 Remove obsolete patterns

Identify and replace:

- Deprecated APIs.
- Outdated Rust idioms.
- Unnecessary clones.
- Avoidable allocations.
- Redundant conversions.
- Manual implementations already supported by the standard library.
- Overly complex `Option` or `Result` handling.
- Redundant lifetime annotations.
- Unnecessary trait bounds.
- Legacy workarounds for old compiler limitations.

Preserve behavior and avoid speculative micro-optimizations.

---

## 4. Strict Compiler and Clippy Enforcement

Configure the strongest practical lint policy.

Start with the following baseline, adapting its location for a Cargo workspace:

```toml
[lints.rust]
unsafe_code = "forbid"
unused_must_use = "deny"
unused_imports = "deny"
unused_variables = "deny"
unreachable_code = "deny"
unfulfilled_lint_expectations = "deny"

[lints.clippy]
all = { level = "deny", priority = -1 }
pedantic = { level = "deny", priority = -1 }

unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
unimplemented = "deny"
dbg_macro = "deny"

indexing_slicing = "deny"
string_slice = "deny"
cast_possible_truncation = "deny"
cast_possible_wrap = "deny"
cast_sign_loss = "deny"

let_underscore_must_use = "deny"
let_underscore_future = "deny"
map_err_ignore = "deny"
allow_attributes = "deny"
```

Investigate additional restriction lints individually.

Do not enable the entire Clippy restriction group indiscriminately.

Check lint compatibility with the actual compiler version.

If a lint conflicts with legitimate code, use a narrowly scoped, documented exception.

Avoid blanket `#[allow(...)]` attributes.

Prefer `#[expect(..., reason = "...")]` for justified exceptions.

Do not silence warnings merely to make CI pass.

---

## 5. Eliminate Panic-Prone and Incorrect Shortcuts

Audit all production code for:

- `.unwrap()` and `.expect()`
- `panic!()`
- `todo!()` and `unimplemented!()`
- Unchecked indexing and slicing.
- Integer overflow.
- Lossy or unchecked numeric casts.
- Ignored `Result` values.
- Silent fallback behavior.
- Incorrect error propagation.
- Partial I/O handling.
- Invalid UTF-8 assumptions.
- Unsafe assumptions about untrusted input.
- Detached tasks with ignored failures.

Replace problematic patterns with explicit and correct handling.

Use typed errors where appropriate.

Prefer `thiserror` for structured library errors and `anyhow` where appropriate at application boundaries.

Avoid unnecessary error boxing or stringification.

Do not replace `unwrap()` with `unwrap_or_default()` merely to silence Clippy.

Preserve meaningful error information.

Distinguish recoverable operational errors from violated internal invariants.

---

## 6. Memory Safety and Resource Management Audit

Perform a comprehensive memory and resource-lifecycle audit.

Investigate:

- Unsafe Rust and FFI correctness.
- Undefined behavior.
- Invalid pointer and lifetime assumptions.
- Allocation size overflow.
- Unbounded buffers and queues.
- Unbounded collections.
- Memory retention after disconnection.
- Reference cycles involving `Arc`.
- Leaked sockets and file descriptors.
- Forgotten background tasks.
- Excessive cloning and allocation.
- Memory exhaustion caused by untrusted input.
- Repeated connection/disconnection memory growth.
- Long-running service memory stability.

Pay special attention to IRC clients that send malformed, oversized, or excessive messages.

Ensure slow clients cannot cause unlimited outbound buffering.

Use bounded queues and explicit backpressure where appropriate.

Investigate native dependencies and allocator behavior.

Use Miri, sanitizers, and fuzzing where technically supported.

Do not claim that safe Rust eliminates all memory-exhaustion or resource-lifecycle bugs.

---

## 7. Async, Tokio, and Concurrency Audit

Review all asynchronous code for:

- Incorrect task ownership.
- Detached or orphaned tasks.
- Cancellation safety.
- Deadlocks.
- Lock ordering.
- Locks held across `.await`.
- Logical race conditions.
- Unbounded channels.
- Backpressure failures.
- Task starvation.
- Busy loops.
- Incorrect timeout handling.
- Partial reads and writes.
- Improper shutdown behavior.
- Lost errors.
- Resource cleanup failures.

Prefer structured task ownership and explicit shutdown behavior.

Review whether `JoinSet`, cancellation tokens, or other modern Tokio facilities would simplify task lifecycle management.

Do not introduce new synchronization merely for stylistic reasons.

Preserve existing performance and concurrency semantics.

---

## 8. Upgrade All Dependencies, Including Major Versions

Perform a comprehensive dependency modernization.

### 8.1 Dependency inventory

Inspect:

- Direct dependencies.
- Transitive dependencies.
- Development dependencies.
- Build dependencies.
- Native libraries.
- Optional dependencies.
- Target-specific dependencies.
- Feature flags.

Identify the latest stable published versions.

Use authoritative package sources such as crates.io and official upstream repositories.

### 8.2 Upgrade policy

Upgrade dependencies to the latest suitable stable versions, **including major-version upgrades**.

Do not restrict upgrades to patch or minor versions.

For every major upgrade:

1. Read the upstream migration guide and changelog.
2. Identify breaking API and behavioral changes.
3. Update application code.
4. Update feature flags.
5. Remove deprecated APIs.
6. Run relevant tests.
7. Verify security and performance implications.

Use tools such as `cargo outdated`, `cargo update`, `cargo tree`, and `cargo deny` where appropriate.

Avoid prerelease versions unless explicitly justified.

### 8.3 Dependency cleanup

Identify:

- Unused dependencies.
- Duplicate dependencies.
- Unnecessary feature flags.
- Redundant utility crates.
- Abandoned packages.
- Unmaintained libraries.
- Packages with known vulnerabilities.

Remove unnecessary dependencies.

Prefer well-maintained libraries with appropriate security and compatibility characteristics.

Evaluate whether newer standard-library APIs can replace small dependencies.

### 8.4 Native dependency compatibility

Pay particular attention to:

- SQLite.
- TLS and cryptographic libraries.
- mimalloc or other allocators.
- FFI bindings.
- GNU and musl builds.
- Cross-platform compilation.

Verify that major dependency upgrades do not introduce ABI incompatibilities, unexpected runtime behavior, or deployment regressions.

Do not replace a mature dependency with an inferior alternative merely because its version number is newer.

---

## 9. Software Engineering and Architecture Refactoring

Improve code organization using established software engineering practices.

This is a substantive part of the task, not optional cleanup.

### 9.1 Refactor oversized source files

Identify source files that have grown excessively large.

Use the following as review heuristics, not rigid limits:

- Over 500 lines: review modularity.
- Over 800 lines: strong refactoring candidate.
- Over 1,200 lines: prioritize architectural review.

Split large files into cohesive modules when doing so improves separation of concerns.

Organize modules around clear responsibilities, such as:

- Protocol parsing.
- Connection lifecycle.
- Authentication.
- Session management.
- Network state.
- Persistence.
- Error handling.
- Configuration.
- Application orchestration.

Do not split files arbitrarily just to reduce line counts.

Keep related code together when that produces a clearer design.

### 9.2 Refactor oversized functions

Review functions using these heuristics:

- Over 50 lines: evaluate complexity.
- Over 80 lines: strong refactoring candidate.
- Over 120 lines: prioritize review.

Also examine shorter functions with excessive branching or nesting.

Extract cohesive helper functions.

Prefer descriptive names and clear contracts.

Reduce unnecessary nesting using early returns where appropriate.

Avoid excessive helper fragmentation.

### 9.3 Reduce complexity

Identify and improve:

- Deeply nested conditionals.
- Large match statements with duplicated behavior.
- Functions with too many responsibilities.
- Excessive parameter lists.
- Duplicated business logic.
- Implicit state transitions.
- Complex mutable shared state.
- Unclear ownership.
- Circular module dependencies.
- Overly generic abstractions.
- Repeated error-handling boilerplate.

Use appropriate abstractions only when they improve clarity.

Avoid premature generalization.

### 9.4 Improve API and type design

Prefer:

- Strongly typed state.
- Explicit invariants.
- Meaningful enums.
- Newtypes where they prevent invalid values.
- Small, cohesive interfaces.
- Clear ownership boundaries.
- Encapsulation of implementation details.
- Explicit state transitions.
- Well-defined error types.

Reduce reliance on loosely structured strings, magic numbers, and boolean parameters when more expressive types materially improve correctness.

Do not introduce unnecessary trait hierarchies or dynamic dispatch.

### 9.5 Documentation

Improve documentation for:

- Public APIs.
- Complex algorithms.
- Non-obvious invariants.
- Async cancellation behavior.
- Unsafe code, if unavoidable.
- Protocol assumptions.
- Database transactions.
- Configuration constraints.

Comments should explain why, not merely repeat what the code does.

Keep documentation consistent with implementation.

---

## 10. Testing and Verification

Strengthen the test suite with:

- Unit tests.
- Integration tests.
- Property-based tests.
- Regression tests.
- Parser fuzzing.
- Concurrency tests.
- Shutdown and cancellation tests.
- Resource-lifecycle tests.
- Database migration tests.
- Protocol compatibility tests.

Test malformed and adversarial input.

Test important invariants, including:

- No silent message loss.
- Correct state transitions.
- Bounded memory growth.
- Proper task cleanup.
- Correct persistence.
- Correct error propagation.
- Stable protocol behavior.

Use `proptest`, `cargo-fuzz`, Miri, and sanitizers where appropriate.

Use Loom for selected concurrency primitives when practical.

Do not add superficial tests merely to increase coverage numbers.

Prioritize tests that detect realistic failures.

---

## 11. Performance Preservation

Correctness and maintainability improvements must not introduce unnecessary performance regressions.

Identify performance-sensitive code, including:

- IRC message parsing.
- Message routing.
- Network I/O.
- Database access.
- History playback.
- Serialization.
- Buffer management.
- Connection handling.

Avoid:

- Unnecessary allocations.
- Excessive cloning.
- Repeated parsing.
- Redundant locking.
- Excessive dynamic dispatch.
- Unbounded task spawning.
- Unnecessary intermediate collections.

Where a refactor affects a hot path, benchmark before and after when practical.

Report meaningful performance changes.

Never sacrifice correctness for speculative performance improvements.

---

## 12. CI and Automated Quality Gates

Implement strict CI enforcement.

Required baseline:

```bash
cargo fmt --all -- --check

cargo check --workspace --all-targets --all-features --locked

cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

cargo test --workspace --all-features --locked

cargo doc --workspace --all-features --no-deps
```

Also evaluate:

- `cargo audit`
- `cargo deny`
- `cargo machete`
- Miri
- Fuzzing
- AddressSanitizer
- LeakSanitizer
- Dependency freshness checks

Run expensive checks in dedicated or scheduled workflows where appropriate.

Verify existing release builds.

Ensure GNU and musl support remains intact.

Do not introduce unstable CI configurations or silently skip failed checks.

---

## 13. Refactoring and Migration Safety

Perform changes incrementally.

Recommended order:

**Phase 1 — Baseline and audit**

Inspect repository, run existing checks, identify risks.

**Phase 2 — Toolchain modernization**

Upgrade to Rust 1.100 if available, evaluate Edition 2024, and migrate deprecated language patterns.

**Phase 3 — Dependency upgrades**

Upgrade direct and transitive dependencies, including major versions, and resolve breaking changes.

**Phase 4 — Strict compiler enforcement**

Configure lints and resolve violations correctly.

**Phase 5 — Memory and concurrency hardening**

Fix verified resource, async, and safety problems.

**Phase 6 — Architecture refactoring**

Refactor large files, large functions, duplicated logic, and unnecessarily complex modules.

**Phase 7 — Test expansion**

Add regression, property-based, integration, and fuzzing coverage.

**Phase 8 — CI hardening**

Enforce all practical quality gates.

**Phase 9 — Final verification**

Run the complete test suite and supported release builds.

After every significant phase, verify that the repository remains functional.

Do not combine unrelated breaking changes into one unreviewable modification.

---

## 14. Non-Negotiable Engineering Rules

1. Fix root causes rather than symptoms.
2. Never silence warnings without technical justification.
3. Never replace errors with arbitrary defaults.
4. Never delete tests merely to achieve passing CI.
5. Never introduce unnecessary `unsafe`.
6. Never weaken authentication or security validation.
7. Never introduce unbounded resource usage.
8. Never change database formats without a justified migration.
9. Never change protocol behavior unintentionally.
10. Never use obsolete APIs when a better stable replacement is appropriate.
11. Never upgrade dependencies without testing breaking changes.
12. Never refactor merely to satisfy arbitrary line-count thresholds.
13. Never introduce speculative abstractions.
14. Never claim tests passed unless actually executed.
15. Never claim a memory-safety proof based solely on compiler or Clippy success.
16. Do not hide failed checks or incomplete work.
17. Preserve backward compatibility unless a change is required and explicitly documented.
18. Favor maintainable, idiomatic Rust over clever code.

---

## 15. Final Report

After implementation, produce a structured report containing:

### A. Rust modernization

- Original and final Rust versions.
- Edition changes.
- New stable language and library features adopted.
- Deprecated patterns removed.

### B. Dependencies

- Upgraded dependencies.
- Major-version migrations.
- Removed dependencies.
- Remaining outdated dependencies and reasons.

### C. Correctness and safety

- Verified bugs fixed.
- Potential risks mitigated.
- Memory and resource-management improvements.
- Async and concurrency improvements.
- Remaining known risks.

### D. Architecture

- Large files refactored.
- Large functions simplified.
- Modules introduced or reorganized.
- Duplicated logic removed.
- Complexity reductions.

### E. Testing

- New tests added.
- Existing tests preserved.
- Fuzzing and sanitizer results.
- Commands executed and results.

### F. CI

- New enforcement rules.
- Compiler and Clippy settings.
- Build compatibility results.
- Any justified exceptions.

### G. Performance

- Performance-sensitive changes.
- Benchmark results, where available.
- Known performance tradeoffs.

### H. Remaining work

List unresolved issues by severity.

Distinguish confirmed bugs, suspected risks, optional improvements, and blocked changes.

---

## 16. Execution Instructions for Codex

**Do not stop after producing an audit report or a proposed plan.**

Proceed with implementation, subject to the constraints below.

- Inspect the repository before editing.
- Work through all phases systematically.
- Use official Rust and dependency documentation.
- Apply changes directly to the repository.
- Keep changes cohesive and reviewable.
- Run checks after meaningful changes.
- Fix failures rather than suppressing them.
- Preserve compatibility and performance.
- Document technical tradeoffs.
- Report blockers transparently.
- Do not fabricate verification results.

If a major upgrade or refactor cannot be completed safely, explain the blocker, preserve the last verified working state, and report the remaining work.

Do not automatically discard unrelated existing changes.

Do not commit, push, publish releases, or modify remote infrastructure unless explicitly authorized.

## Final Success Criteria

The completed repository should:

- Use the latest appropriate stable Rust features through version 1.100.
- Use Rust Edition 2024 where practical.
- Have modern, maintained dependencies, including completed major upgrades where feasible.
- Enforce strict compiler and Clippy correctness rules.
- Avoid panic-prone and unchecked shortcuts.
- Have stronger memory and resource safety.
- Have reliable async task lifecycle management.
- Have clear, modular, maintainable architecture.
- Avoid unnecessarily large files and functions.
- Have strong automated tests and CI enforcement.
- Preserve IRC behavior, database integrity, and deployment compatibility.
- Avoid significant performance regressions.

**Deliver actual verified code improvements, not merely recommendations or superficial lint compliance.**

---

## 17. Mandatory Audit Report and Step-by-Step Implementation Plan

Write all audit findings and the proposed implementation plan to a Markdown file named **`doc/audit_results.md`** in the repository documentation directory.

This file is a mandatory deliverable.

### 17.1 Audit findings

Document the results of the complete repository audit, including:

- Rust compiler, edition, and language modernization opportunities through Rust 1.100.
- Outdated dependencies, including major-version upgrades.
- Compiler and Clippy violations.
- Potential correctness, memory-safety, and resource-management bugs.
- Async concurrency and cancellation risks.
- Oversized source files and functions requiring refactoring.
- Architectural weaknesses and duplicated logic.
- Missing tests and insufficient coverage.
- CI/CD weaknesses.
- Performance and compatibility risks.

For every finding, include:

1. **Finding ID:** Unique identifier (e.g., `RUST-001`, `MEM-001`, `ARCH-001`).
2. **Severity:** Critical / High / Medium / Low.
3. **Location:** Exact file path, function, and line numbers where possible.
4. **Problem:** Clear technical explanation.
5. **Impact:** Correctness, security, memory, performance, or maintainability.
6. **Recommended fix:** Specific actionable changes.
7. **Verification:** Tests or commands needed to confirm the fix.
8. **Status:** Not Started / In Progress / Completed / Blocked.

Distinguish confirmed defects from potential risks and optional improvements.

### 17.2 Step-by-step implementation plan

Create a detailed, prioritized implementation plan inside `doc/audit_results.md`.

Organize the plan into sequential phases:

1. Baseline verification and critical bug fixes.
2. Rust toolchain and Edition 2024 modernization.
3. Dependency upgrades, including major versions.
4. Strict compiler and Clippy enforcement.
5. Memory-safety and resource-lifecycle improvements.
6. Async and concurrency correctness.
7. Architectural refactoring of large files and functions.
8. Testing, fuzzing, and sanitizer coverage.
9. CI/CD hardening.
10. Final regression testing and production readiness review.

For **every implementation step**, specify:

- Step ID and priority.
- Related audit finding IDs.
- Objective and technical rationale.
- Exact files or modules to modify.
- Detailed implementation instructions.
- Dependencies on earlier steps.
- Expected compatibility or performance risks.
- Required tests and verification commands.
- Measurable acceptance criteria.
- Completion status.

Break large tasks into small, independently verifiable steps. Order tasks by dependencies, risk, and implementation priority.

Use Markdown checkboxes to track progress:

```markdown
- [ ] STEP-001: Establish compiler and test baseline
- [ ] STEP-002: Fix critical correctness defects
- [ ] STEP-003: Upgrade Rust toolchain
- [ ] STEP-004: Upgrade dependencies
```

### 17.3 Verification and progress tracking

Include a verification matrix covering:

- `cargo fmt`
- `cargo check`
- `cargo clippy -D warnings`
- `cargo test`
- `cargo doc`
- Release builds
- GNU/musl compatibility
- Miri, where supported
- Sanitizers, where supported
- Fuzzing and regression tests

Record actual results as **PASS / FAIL / BLOCKED / NOT RUN**.

Never report a check as passed without executing it.

Update `doc/audit_results.md` after each completed implementation phase, recording:

- Changes made.
- Bugs fixed.
- Tests executed.
- Verification results.
- Remaining issues.
- Any deviations from the original plan.

### 17.4 Required document structure

The final `doc/audit_results.md` must contain:

```markdown
# soju-rs — Comprehensive Rust Audit and Modernization

## 1. Executive Summary
## 2. Repository and Toolchain Baseline
## 3. Audit Findings
### 3.1 Critical Issues
### 3.2 High-Priority Issues
### 3.3 Medium-Priority Issues
### 3.4 Low-Priority Issues

## 4. Rust 1.100 and Edition 2024 Modernization
## 5. Dependency Upgrade Assessment
## 6. Compiler and Clippy Hardening
## 7. Memory Safety and Resource Management
## 8. Async and Concurrency Correctness
## 9. Architecture and Refactoring Opportunities
## 10. Testing and CI Assessment

## 11. Step-by-Step Implementation Plan
### Phase 1 — Baseline and Critical Fixes
### Phase 2 — Rust Modernization
### Phase 3 — Dependency Upgrades
### Phase 4 — Compiler and Clippy
### Phase 5 — Memory and Resource Safety
### Phase 6 — Async and Concurrency
### Phase 7 — Architecture Refactoring
### Phase 8 — Testing and Fuzzing
### Phase 9 — CI Hardening
### Phase 10 — Final Verification

## 12. Verification Matrix
## 13. Compatibility and Performance Risks
## 14. Remaining Issues and Blockers
## 15. Implementation Progress Log
## 16. Final Recommendations
```

### 17.5 Execution order — mandatory

**For this run, perform the audit and create the implementation plan only. Do not implement code changes yet.**

1. Inspect the entire repository.
2. Execute available baseline checks.
3. Research Rust modernization and dependency upgrades.
4. Identify and prioritize findings.
5. Create `doc/audit_results.md` in the repository documentation directory.
6. Write the complete step-by-step implementation plan.
7. Verify that the document includes actionable tasks, dependencies, acceptance criteria, and verification commands.
8. Stop and present the audit summary and implementation plan for review.

Do not modify Rust source code, Cargo manifests, dependencies, or CI workflows during this audit-only phase.

**The next Codex execution should use `doc/audit_results.md` as the authoritative implementation checklist, execute the plan phase by phase, and update the document as work progresses.**

Do not claim that an issue has been fixed until the implementation and its verification are complete.
