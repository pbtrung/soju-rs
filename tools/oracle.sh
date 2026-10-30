#!/bin/bash
# Builds the differential oracle (tests/oracle) against soju-rs main at the
# revision the port follows (PLAN.md), into build/oracle/release/soju-oracle.
# The soju-rs sources are a git worktree in build/soju-main.
set -euo pipefail
cd "$(dirname "$0")/.."
rev="${SOJU_RS_REV:-6b9bf15}"
if [[ ! -e build/soju-main/.git ]]; then
	mkdir -p build
	git worktree add --detach build/soju-main "$rev" >/dev/null
fi
git -C build/soju-main checkout -q --detach "$rev"
cargo build --release --locked --manifest-path tests/oracle/Cargo.toml --target-dir build/oracle
