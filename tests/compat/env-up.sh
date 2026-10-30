#!/bin/bash
# Starts two identical ergo servers (16667 for soju-rs, 16668 for Go soju),
# soju-rs (16670 irc, 16671 ws) and Go soju (16680 irc, 16681 ws), with fresh
# databases and users alice (admin) and bob.
set -e
export SOJU_LOOPS=${SOJU_LOOPS:-4}
export TOKIO_WORKER_THREADS=${TOKIO_WORKER_THREADS:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
S=${COMPAT_WORK:-$HERE/work}
RSBIN=${RSBIN:-$HERE/../../build/oracle/release}
CBIN=${CBIN:-$HERE/../../build/clang-asan}
# Only stop the processes started by a previous run, recorded in pid files,
# if they're still what was started there
stop() { # dir
  local pid
  [ -f "$1/pid" ] || return 0
  pid=$(cat "$1/pid")
  if [ "$(readlink "/proc/$pid/cwd")" = "$1" ]; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 600); do
      [ "$(readlink "/proc/$pid/cwd")" != "$1" ] && break
      sleep 0.1
    done
    if [ "$(readlink "/proc/$pid/cwd")" = "$1" ]; then
      echo "shutdown timed out: $1" >&2; exit 1
    fi
    if [ -f "$1/soju.log" ] && grep -Eq 'ERROR: .*Sanitizer|runtime error:|live objects:|ERROR SUMMARY: [1-9]|definitely lost: [1-9]|indirectly lost: [1-9]' "$1/soju.log"; then
      cat "$1/soju.log" >&2; exit 1
    fi
  fi
  rm -f "$1/pid"
}
for d in "$S/env/ergo-16667" "$S/env/ergo-16668" "$S/env/ergo-16669" "$S/env/rs" "$S/env/go" "$S/env/c"; do
  stop "$d"
done
sleep 0.5
for port in 16667 16668 16669; do
  D=$S/env/ergo-$port
  rm -rf "$D"; mkdir -p "$D"
  ln -s "$S/src/ergo/languages" "$D/languages"
  sed -e "s/127.0.0.1:16667/127.0.0.1:$port/" -e "s/TLSPORT/$((port+30))/" -e "s#$S/env/ergo/ircd.db#$D/ircd.db#" "$S/env/ergo/ircd.yaml" > "$D/ircd.yaml"
  (cd "$D" && "$S/bin/ergo" initdb --conf ircd.yaml >/dev/null 2>&1)
  (cd "$D" && exec "$S/bin/ergo" run --conf ircd.yaml > ergo.log 2>&1) &
  echo $! > "$D/pid"
done
python3 - <<'READY'
import socket, time
for port in (16667, 16668, 16669):
    deadline = time.monotonic() + 60
    while True:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                break
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(.1)
READY
start() { # name bin ircport wsport
  local D=$S/env/$1
  rm -rf "$D"; mkdir -p "$D"
  cat > $D/config <<CFG
db sqlite3 $D/soju.db
hostname soju.test
listen irc+insecure://127.0.0.1:$3
listen ws+insecure://127.0.0.1:$4
listen unix+admin://$D/admin
http-origin *
${EXTRA_CONFIG//%D%/$D}
CFG
  (cd "$D" && exec "$2/soju" -config "$D/config" ${SOJU_FLAGS:-} > "$D/soju.log" 2>&1) &
  echo $! > "$D/pid"
  for i in $(seq 600); do [ -S "$D/admin" ] && break; sleep 0.1; done
  "$2/sojuctl" -config "$D/config" user create -username alice -password secret -admin >/dev/null
  "$2/sojuctl" -config "$D/config" user create -username bob -password secret >/dev/null
}
start rs "$RSBIN" 16670 16671
start go "$S/bin" 16680 16681
start c "$CBIN" 16690 16691
sleep 0.5
echo "env up"
