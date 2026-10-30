#!/bin/bash
# Prepares the compatibility test environment in $COMPAT_WORK (default:
# ./work): builds upstream Go soju and the ergo IRC server from source,
# generates TLS certificates and the ergo configuration template.
#
# Requires git, go, openssl and python3.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
S=${COMPAT_WORK:-$HERE/work}
SOJU_REF=${SOJU_REF:-master}
mkdir -p "$S/bin" "$S/src" "$S/env/ergo" "$S/env/certs"

if [ ! -d "$S/src/soju" ]; then
	git clone -q https://codeberg.org/emersion/soju.git "$S/src/soju"
fi
git -C "$S/src/soju" checkout -q "$SOJU_REF"
(cd "$S/src/soju" && go build -o "$S/bin/" ./cmd/soju ./cmd/sojuctl)

if [ ! -d "$S/src/ergo" ]; then
	git clone -q --depth 1 https://github.com/ergochat/ergo.git "$S/src/ergo"
fi
(cd "$S/src/ergo" && go build -o "$S/bin/ergo" .)

cd "$S/env/certs"
if [ ! -f cert.pem ]; then
	openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
		-keyout key.pem -out cert.pem -days 3650 -subj "/CN=localhost" \
		-addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null
	openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
		-keyout client-key.pem -out client-cert.pem -days 3650 -subj "/CN=client" 2>/dev/null
fi

# ergo configuration template: plain-text listener on 16667 and a TLS one
# on TLSPORT (rewritten per instance by env-up.sh), no fakelag
python3 - "$S" <<'PY'
import sys
S = sys.argv[1]
s = open(f"{S}/src/ergo/default.yaml").read()
start = s.index("    listeners:\n") + len("    listeners:\n")
end = s.index("        # Example of a Unix domain socket for proxying:")
s = (s[:start] + '        "127.0.0.1:16667":\n'
     f'        "127.0.0.1:TLSPORT":\n            tls:\n                cert: {S}/env/certs/cert.pem\n'
     f'                key: {S}/env/certs/key.pem\n\n' + s[end:])
s = s.replace("path: ircd.db", f"path: {S}/env/ergo/ircd.db")
i = s.index("\nfakelag:")
j = s.index("enabled: true", i)
s = s[:j] + "enabled: false" + s[j + len("enabled: true"):]
open(f"{S}/env/ergo/ircd.yaml", "w").write(s)
PY
# gamja, for the end-to-end tests: a local checkout (GAMJA_SRC) or upstream
if [ ! -d "$S/gamja/node_modules" ]; then
	rm -rf "$S/gamja"
	if [ -n "${GAMJA_SRC:-}" ]; then
		mkdir -p "$S/gamja"
		(cd "$GAMJA_SRC" && git ls-files -z | xargs -0 -I{} cp --parents {} "$S/gamja/")
	else
		git clone -q --depth 1 https://codeberg.org/emersion/gamja.git "$S/gamja"
	fi
	(cd "$S/gamja" && npm install --no-audit --no-fund >/dev/null)
fi
(cd "$S/gamja" && npm install --no-save --no-audit --no-fund @playwright/test@1.58.2 && npm run build)
mkdir -p "$S/gamja/e2e-real"
cp "$HERE/gamja/soju.spec.ts" "$S/gamja/e2e-real/"
cp "$HERE/gamja/playwright.real.config.ts" "$S/gamja/"

echo "compat environment ready in $S"
