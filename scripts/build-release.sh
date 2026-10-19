#!/bin/bash
# Builds release archives for all supported targets into dist/, using the
# Docker image from Dockerfile.release.
#
# Usage: scripts/build-release.sh [version]
set -euo pipefail

cd "$(dirname "$0")/.."
version="${1:-v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
image="soju-rs-release"

# glibc builds target glibc 2.28 for compatibility with older distributions
targets=(
	"x86_64-unknown-linux-gnu.2.28:x86_64-unknown-linux-gnu"
	"aarch64-unknown-linux-gnu.2.28:aarch64-unknown-linux-gnu"
	"x86_64-unknown-linux-musl:x86_64-unknown-linux-musl"
	"aarch64-unknown-linux-musl:aarch64-unknown-linux-musl"
)

docker build -f Dockerfile.release -t "$image" .

mkdir -p dist
rm -f dist/*.tar.gz dist/SHA256SUMS

docker run --rm \
	-u "$(id -u):$(id -g)" \
	-e CARGO_HOME=/src/target/cargo-home \
	-e HOME=/tmp \
	-v "$PWD:/src" \
	"$image" \
	bash -euo pipefail -c '
		mkdir -p "$CARGO_HOME"
		for spec in '"${targets[*]}"'; do
			zig_target="${spec%%:*}"
			cargo zigbuild --locked --release --target "$zig_target"
		done
	'

for spec in "${targets[@]}"; do
	target="${spec##*:}"
	name="soju-rs-${version}-${target}"
	staging="dist/$name"
	rm -rf "$staging"
	mkdir -p "$staging"
	cp "target/$target/release/soju" "target/$target/release/sojuctl" "$staging/"
	cp LICENSE README.md config.in "$staging/"
	tar -C dist -czf "dist/$name.tar.gz" "$name"
	rm -rf "$staging"
done

(cd dist && sha256sum ./*.tar.gz | sed 's# \./# #' > SHA256SUMS)
ls -l dist
