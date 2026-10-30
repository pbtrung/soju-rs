#!/bin/bash
# Ten minutes by default; requires tests/compat/setup.sh's two real Ergo servers.
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 tools/soak.py "$@"
