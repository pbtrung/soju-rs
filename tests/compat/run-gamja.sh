#!/bin/bash
# Runs the gamja e2e suite against rs and/or go (fresh environment). Exits
# with a non-zero status if the environment fails to start or any run fails.
HERE=$(cd "$(dirname "$0")" && pwd)
S=${COMPAT_WORK:-$HERE/work}
if [ $# -eq 0 ]; then
  echo "usage: $0 <rs|go>..." >&2
  exit 2
fi
for kind in "$@"; do
  case $kind in
    rs|go|c) ;;
    *) echo "unknown bouncer: $kind" >&2; exit 2 ;;
  esac
done
bash "$HERE/env-up.sh" > /dev/null || { echo "failed to start the environment" >&2; exit 1; }
cd "$S/gamja" || exit 1
status=0
for kind in "$@"; do
  if [ "$kind" = rs ]; then ws=16671; ergo=16667; elif [ "$kind" = c ]; then ws=16691; ergo=16669; else ws=16681; ergo=16668; fi
  SOJU_WS=ws://127.0.0.1:$ws/socket ERGO_PORT=$ergo timeout 600 npx playwright test -c playwright.real.config.ts --output "test-results-$kind" > "$S/gamja-$kind.txt" 2>&1 || status=1
  echo "== $kind"; grep -E '✓|✘|passed|failed|did not run' "$S/gamja-$kind.txt"
done
exit $status
