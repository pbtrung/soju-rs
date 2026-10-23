#!/bin/bash
# Check project sources, leaving vendored code to its upstream maintainers.
set -euo pipefail
cd "$(dirname "$0")/.."
mapfile -t files < <(rg --files src tests fuzz spikes -g '*.c' -g '*.h')
clang-format --dry-run --Werror "${files[@]}"
if rg -n '\b(strcpy|strcat|sprintf|strtok|gets|alloca|atoi)\s*\(' src tests fuzz spikes; then
	echo 'use checked allocation and string helpers (doc/conventions.md)' >&2
	exit 1
fi
for f in "${files[@]}"; do
	[[ "$f" == *.c ]] || continue
	clang-tidy -p build/clang-asan "$f"
done
