#!/bin/bash
# Runs the differential harness, writing the full report to difflatest.txt
# and printing a summary.
HERE=$(cd "$(dirname "$0")" && pwd)
S=${COMPAT_WORK:-$HERE/work}
cd $HERE && python3 diffharness.py "$@" > $S/difflatest.txt 2>&1
grep -E '^(PASS|FAIL)|failed:' $S/difflatest.txt
