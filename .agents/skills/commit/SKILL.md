---
name: commit
description: Prepare and commit soju-rs changes with relevant checks and a detailed message without AI attribution; push when the user requests the commit-and-push workflow. Use for repository commit requests, not merely creating or editing this skill.
---

# Commit changes

Adapted from [.claude/commands/commit.md](../../../.claude/commands/commit.md).
Read [AGENTS.md](../../../AGENTS.md) for repository constraints and verification.

Invoking `$commit` requests the commit-and-push workflow unless the user says
local-only. Honor narrower instructions: a request for only a local commit
does not authorize pushing. Creating or editing this skill does not authorize
any commit, push or release operation.

## Review scope and check changes

1. Read `git status --short --branch`, `git diff`, and `git diff --cached`.
   Inspect relevant untracked files too; they are absent from ordinary diff.
   Establish what belongs in the requested commit before changing the index.
2. If the user already staged changes, preserve that boundary unless asked
   to include more. Do not unstage, discard or incorporate unrelated work.
   For partially staged files, avoid staging the whole file just to include
   formatting changes. Resolve genuinely ambiguous scope before committing.
3. Check the actual proposed content:

   | Changed content | Verification |
   | --- | --- |
   | Rust, `Cargo.toml`, `Cargo.lock` | `cargo fmt --all`, then the check/Clippy/test commands in AGENTS.md, including all-target and doc tests |
   | Python under `tests/` | Ruff format/check on touched files if installed; relevant compatibility scenarios when provisioned |
   | Shell scripts | `bash -n` on each changed shell script; ShellCheck if installed |
   | Dockerfiles, if present | Appropriate available Dockerfile validation; do not pass Dockerfiles to `bash -n` |
   | Markdown/docs | Relative links/anchors and stale references; rustdoc for Rust doc comments |

   Run `git diff --check` and `git diff --cached --check`. Fix relevant
   failures and rerun affected checks. Review formatter output before staging.
   Do not suppress Clippy across the crate to obtain a pass. Report unavailable
   optional tools or environmental blockers; never report unrun checks as passed.
4. Exclude `target/`, `dist/`, local databases (`*.db`, `*.db-wal`, `*.db-shm`),
   TLS private keys and credentials, including accidentally staged artifacts.
   If such files are already staged, stop and identify the issue before
   committing; do not silently reset the user's index.

## Stage and commit

When nothing is staged, stage the relevant modified/new/deleted paths explicitly
with `git add -- <paths>`. Avoid blanket staging that absorbs unrelated files.
When the index is already populated, add further paths only within the user's
stated scope. Inspect the final staged diff, including moves and new files.
If there is no relevant change, report that rather than creating an empty commit.

Write a detailed message based only on the staged diff:

- Use a concise imperative subject, such as “Fix”, “Add” or “Refactor”.
- Explain what changed and why in the body; use bullets for distinct changes.
- Include meaningful verification or known limitations when useful.
- Exclude conversational history, abandoned attempts and unsupported claims.
- Do not include AI attribution, generated-by lines or AI co-author trailers.

Write the exact message to a temporary UTF-8 file, then run
`git commit --file <message-file>` so newlines are preserved without shell
interpolation. Preserve hooks and signing configuration. If a hook changes
files or fails, inspect the result and resolve it within scope; do not bypass
hooks or amend an unrelated existing commit.

## Push and confirm

For an authorized commit-and-push workflow, push after committing. Inspect
the current branch and upstream first; use `git push` when tracking is configured.
If no upstream exists, use `git push -u origin <current-branch>` only when
`origin` is the intended destination. Detached HEAD or an ambiguous destination
needs clarification before pushing.

If push fails, retain the local commit and report the actual error. Do not
force-push, rebase, reset, merge or retry with different remote/ref settings
without authorization for that recovery. A push does not authorize running
release helpers or publishing a release.

After completion inspect `git log -1` and `git status --short --branch`.
Report the commit hash/subject, checks actually run, push outcome (or local-only
status), and any remaining changes. Never imply a failed push succeeded.
