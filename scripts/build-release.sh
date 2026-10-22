#!/bin/bash
# Builds release archives for all supported targets into dist/: glibc
# targets in an Arch Linux image (Dockerfile.release-gnu), musl targets in
# an Alpine edge image (Dockerfile.release-musl). Both images are rebuilt
# from freshly pulled bases, upgrading all packages before building.
#
# Usage: scripts/build-release.sh [version]
set -euo pipefail

cd "$(dirname "$0")/.."
version="${1:-v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"

# image:target
targets=(
	"gnu:x86_64-unknown-linux-gnu"
	"gnu:aarch64-unknown-linux-gnu"
	"musl:x86_64-unknown-linux-musl"
	"musl:aarch64-unknown-linux-musl"
)

for flavor in gnu musl; do
	docker build --pull --no-cache -f "Dockerfile.release-$flavor" -t "soju-rs-release-$flavor" .
done

mkdir -p dist
rm -f dist/*.tar.gz dist/SHA256SUMS

for spec in "${targets[@]}"; do
	flavor="${spec%%:*}"
	target="${spec#*:}"
	docker run --rm \
		-u "$(id -u):$(id -g)" \
		-e CARGO_HOME=/src/target/cargo-home \
		-e HOME=/tmp \
		-v "$PWD:/src" \
		"soju-rs-release-$flavor" \
		bash -euo pipefail -c '
			mkdir -p "$CARGO_HOME"
			cargo zigbuild --locked --release --target "$1"
		' bash "$target"
done

for spec in "${targets[@]}"; do
	target="${spec#*:}"
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
