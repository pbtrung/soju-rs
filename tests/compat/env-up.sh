#!/bin/bash
# Starts two identical ergo servers (16667 for soju-rs, 16668 for Go soju),
# soju-rs (16670 irc, 16671 ws) and Go soju (16680 irc, 16681 ws), with fresh
# databases and users alice (admin) and bob.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
S=${COMPAT_WORK:-$HERE/work}
RSBIN=${RSBIN:-$HERE/../../target/debug}
pkill -x ergo || true
pkill -f "soju -config $S/env/go/config" || true
pkill -f "soju -config $S/env/rs/config" || true
sleep 0.5
for port in 16667 16668; do
  D=$S/env/ergo-$port
  rm -rf $D; mkdir -p $D
  ln -s $S/src/ergo/languages $D/languages
  sed -e "s/127.0.0.1:16667/127.0.0.1:$port/" -e "s/TLSPORT/$((port+30))/" -e "s#$S/env/ergo/ircd.db#$D/ircd.db#" $S/env/ergo/ircd.yaml > $D/ircd.yaml
  (cd $D && $S/bin/ergo initdb --conf ircd.yaml >/dev/null 2>&1 && $S/bin/ergo run --conf ircd.yaml > ergo.log 2>&1 &)
done
start() { # name bin ircport wsport ergoport
  local D=$S/env/$1
  rm -rf $D; mkdir -p $D
  cat > $D/config <<CFG
db sqlite3 $D/soju.db
hostname soju.test
listen irc+insecure://127.0.0.1:$3
listen ws+insecure://127.0.0.1:$4
listen unix+admin://$D/admin
http-origin *
${EXTRA_CONFIG//%D%/$D}
CFG
  $2/soju -config $D/config ${SOJU_FLAGS:-} > $D/soju.log 2>&1 &
  for i in $(seq 50); do [ -S $D/admin ] && break; sleep 0.1; done
  $2/sojuctl -config $D/config user create -username alice -password secret -admin >/dev/null
  $2/sojuctl -config $D/config user create -username bob -password secret >/dev/null
}
start rs $RSBIN 16670 16671
start go $S/bin 16680 16681
sleep 0.5
echo "env up"
