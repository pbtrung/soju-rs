#!/bin/bash
# Runs the differential harness, writing the full report to difflatest.txt
# and printing a summary. Exits with the harness's status.
HERE=$(cd "$(dirname "$0")" && pwd)
S=${COMPAT_WORK:-$HERE/work}
cd "$HERE" || exit 1
python3 diffharness.py "$@" > "$S/difflatest.txt" 2>&1
status=$?
grep -E '^(PASS|FAIL)|failed:' "$S/difflatest.txt"
exit $status
