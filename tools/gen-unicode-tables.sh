#!/bin/bash
# Regenerates src/base/unicode_tables.inc with the local Rust toolchain
# (use the one soju-rs is built with).
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
SOJU_RUSTC_VERSION="$(rustc --version)" rustc -O --edition 2024 -o "$tmp/gen" tools/gen-unicode-tables.rs
"$tmp/gen" >src/base/unicode_tables.inc
