#!/bin/bash
# Per-file line counts and per-function length/complexity of the C sources
# (doc/audit-plan.md phase A). Needs lizard (pip install lizard), looked up
# in PATH or in build/venv.
# Lists the files and functions over the audit limits (500 lines; 80
# lines or cyclomatic complexity 15).
set -euo pipefail
cd "$(dirname "$0")/.."
lizard=$(command -v lizard || echo build/venv/bin/lizard)
[[ -x "$lizard" ]] || { echo 'lizard not found (pip install lizard)' >&2; exit 2; }
echo '## Files over 500 lines'
find src tests fuzz -name '*.[ch]' -print0 | xargs -0 wc -l | grep -v ' total$' |
	sort -rn | awk '$1 > 500 { printf "| %s | %d |\n", $2, $1 }'
echo
echo '## Functions over 80 lines or cyclomatic complexity 15'
"$lizard" --csv src tests fuzz 2>/dev/null |
	awk -F, '$2 + 0 > 15 || $5 + 0 > 80 {
		gsub(/"/, "", $7); gsub(/"/, "", $8)
		printf "| %s | %s | %d | %d |\n", $7, $8, $5, $2 }' | sort
