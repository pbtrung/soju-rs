#!/bin/bash
# Build x86_64 GNU and static musl archives with local toolchains.
# Requires cargo, both Rust targets, a native C compiler, musl-gcc, cmake,
# make, perl, tar and sha256sum. No container runtime is used.
#
# Usage: scripts/build-release.sh [version]
# Override CC_x86_64_unknown_linux_musl for a local musl compiler wrapper.
# CARGO_TARGET_DIR and MUSL_TARGET_DIR optionally select build directories.
set -euo pipefail

cd "$(dirname "$0")/.."
package_version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
version="${1:-v${package_version}}"
if [[ $# -gt 1 || "$version" != "v${package_version}" ]]; then
	printf 'Version must match Cargo.toml: v%s\n' "$package_version" >&2
	exit 1
fi

export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"
# Rust supplies its own musl CRT and libc. Its default native linker handles
# static PIE correctly; musl-gcc wrappers may add a dynamic loader to it.
if ! command -v "$CC_x86_64_unknown_linux_musl" >/dev/null; then
	printf 'Missing musl C compiler: %s (install musl or set CC_x86_64_unknown_linux_musl)\n' "$CC_x86_64_unknown_linux_musl" >&2
	exit 1
fi

targets=(x86_64-unknown-linux-gnu x86_64-unknown-linux-musl)
build_dirs=("${CARGO_TARGET_DIR:-target}" "${MUSL_TARGET_DIR:-${CARGO_TARGET_DIR:-target}}")
for i in "${!targets[@]}"; do
	cargo build --locked --release --bins --target "${targets[$i]}" --target-dir "${build_dirs[$i]}"
done

mkdir -p dist
staging="$(mktemp -d dist/.release.XXXXXX)"
trap 'rm -rf "$staging"' EXIT
archives=()
for i in "${!targets[@]}"; do
	target="${targets[$i]}"
	name="soju-rs-${version}-${target}"
	mkdir -p "$staging/$name"
	cp "${build_dirs[$i]}/$target/release/soju" "${build_dirs[$i]}/$target/release/sojuctl" "$staging/$name/"
	cp -r LICENSE README.md config.in doc "$staging/$name/"
	tar -C "$staging" -czf "dist/$name.tar.gz" "$name"
	archives+=("$name.tar.gz")
done

# Hash only this release's two archives, even if dist contains older builds.
(cd dist && sha256sum "${archives[@]}" > SHA256SUMS)
ls -lh "${archives[@]/#/dist/}" dist/SHA256SUMS
