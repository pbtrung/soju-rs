---
description: Commit staged/modified changes with a detailed message and push, no AI co-author attribution
---

# Commit and Push

## Steps

1. Run `git status` and `git diff` (and `git diff --staged` if anything is already staged) to see all changes.
2. Format, lint and test whatever's actually touched, before staging anything:
   - Any `*.c` or `*.h` changed (outside `deps/`): `clang-format -i` each
     changed file (uses the repo's `.clang-format`), then
     `clang-tidy -p build/debug` on each changed `*.c` file.
   - Any `*.c`, `*.h`, `CMakeLists.txt`, `CMakePresets.json`, `*.cmake` or
     anything under `deps/` changed: build and test the `debug` and `asan`
     modes with both compilers (sections 2.2 and 3.2 of `PLAN.md`), e.g.
     `CC=gcc cmake --preset asan && cmake --build --preset asan && ctest --preset asan`,
     then the same with `CC=clang`. A build warning is an error (`-Werror`),
     and any ASan/UBSan/LSan report fails the check. If the change touches
     the DB thread or threadpool handoff, also run the `tsan` preset.
   - Any `fuzz/**` changed: build the `fuzz` preset (Clang) and run each
     changed harness for 60 s (`-max_total_time=60`).
   - Any `tests/**/*.py` changed: `python3 -m ruff format tests` then
     `python3 -m ruff check tests` (skip if ruff isn't installed).
   - Any `docker/Dockerfile*`, `scripts/*.sh` or `tools/*.sh` changed:
     `bash -n` each changed script, and `shellcheck` it if available.
   - Until the CMake tree exists (phase 0 of `PLAN.md`), skip the C checks.
   - If formatting rewrote a file, or any check reports an error, fix it and
     re-run before continuing.
   - Never commit `build/`, local databases (`*.db`, `*.db-wal`, `*.db-shm`),
     TLS keys, release artifacts (`dist/`), fuzz corpora or crash files
     (`fuzz/corpus/`, `fuzz/crashes/`); they belong in `.gitignore`.
3. If nothing is staged, stage all relevant modified/new files with `git add`.
4. Write a **detailed** commit message:
   - Subject line: concise summary of the change (imperative mood, e.g. "Add", "Fix", "Refactor").
   - Body: explain _what_ changed and _why_, as bullet points if there are multiple distinct changes.
   - Base the message only on the actual diff — do not include conversational back-and-forth, dead ends, or trial-and-error from the session.
5. Create the commit using a HEREDOC so formatting is preserved, e.g.:
   ```bash
   git commit -m "$(cat <<'EOF'
   Short summary of the change

   - Detail one
   - Detail two
   - Why this change was made
   EOF
   )"
   ```
6. **Do not** add any AI attribution — no `🤖 Generated with Claude Code` line, no `Co-Authored-By: Claude` trailer, no mention of Claude/AI anywhere in the message.
7. Push the commit to the current branch's remote (`git push`, or `git push -u origin <branch>` if it has no upstream yet).
8. Confirm success by showing `git log -1` and `git status` after pushing.

## Rules

- Never include Claude/AI co-authorship or attribution in the commit message.
- Always push after committing — don't stop at just the local commit.
- If the push fails (e.g. diverged branch), report the error and ask before force-pushing or rebasing.
- Don't silence warnings or sanitizer findings to get a check passing: no
  project-wide `-Wno-*`, no new entries in `tests/lsan.supp` or
  `tests/valgrind.supp` without a comment linking the upstream library bug.
  Fix the code, or add a narrowly scoped `#pragma GCC diagnostic` /
  `NOLINT(check-name)` with a comment explaining why.
