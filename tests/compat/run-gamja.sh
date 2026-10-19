#!/bin/bash
# Runs the gamja e2e suite against rs or go (fresh environment)
HERE=$(cd "$(dirname "$0")" && pwd)
S=${COMPAT_WORK:-$HERE/work}
bash $HERE/env-up.sh > /dev/null
cd $S/gamja
for kind in "$@"; do
  if [ $kind = rs ]; then ws=16671; ergo=16667; else ws=16681; ergo=16668; fi
  SOJU_WS=ws://127.0.0.1:$ws/socket ERGO_PORT=$ergo timeout 600 npx playwright test -c playwright.real.config.ts --output test-results-$kind > $S/gamja-$kind.txt 2>&1
  echo "== $kind"; grep -E '✓|✘|passed|failed|did not run' $S/gamja-$kind.txt
done
