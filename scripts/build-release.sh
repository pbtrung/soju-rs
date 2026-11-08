#!/bin/bash
# Build x86_64 GNU and static musl archives with local toolchains.
# Requires cargo, both Rust targets, a native C compiler, musl-gcc, cmake,
# make, perl, tar, readelf and sha256sum. No container runtime is used.
#
# Usage: scripts/build-release.sh [version]
# Override CC_x86_64_unknown_linux_musl for a local musl compiler wrapper.
# CARGO_TARGET_DIR and MUSL_TARGET_DIR optionally select build directories.
# DIST_DIR optionally selects the archive directory (default: dist).
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
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-cc}"
for tool in cargo rustc cc "$CC_x86_64_unknown_linux_musl" "$CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER" cmake make perl tar readelf sha256sum; do
	if ! command -v "$tool" >/dev/null; then
		printf 'Missing local build tool: %s\n' "$tool" >&2
		exit 1
	fi
done

targets=(x86_64-unknown-linux-gnu x86_64-unknown-linux-musl)
build_dirs=("${CARGO_TARGET_DIR:-target}" "${MUSL_TARGET_DIR:-${CARGO_TARGET_DIR:-target}}")
# Check installed target libraries directly, supporting distro Rust packages
# such as Arch's rust and rust-musl without requiring rustup.
for target in "${targets[@]}"; do
	libdir="$(rustc --print target-libdir --target "$target")"
	if ! compgen -G "$libdir/libstd-*.rlib" >/dev/null; then
		printf 'Missing installed Rust standard library for %s\n' "$target" >&2
		exit 1
	fi
done
for i in "${!targets[@]}"; do
	cargo build --locked --release --bins --target "${targets[$i]}" --target-dir "${build_dirs[$i]}"
done

# Verify static linkage before writing any archives. In particular, a binary
# with an ELF interpreter still needs a musl loader installed on the host.
for binary in soju sojuctl; do
	path="${build_dirs[1]}/${targets[1]}/release/$binary"
	program_headers="$(readelf --program-headers --wide "$path")"
	dynamic_entries="$(readelf --dynamic --wide "$path")"
	if [[ "$program_headers" == *INTERP* || "$dynamic_entries" == *'(NEEDED)'* ]]; then
		printf 'musl executable is not fully static: %s\n' "$path" >&2
		exit 1
	fi
done

dist_dir="${DIST_DIR:-dist}"
mkdir -p "$dist_dir"
staging="$(mktemp -d "$dist_dir/.release.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
archives=()
for i in "${!targets[@]}"; do
	target="${targets[$i]}"
	name="soju-rs-${version}-${target}"
	mkdir -p "$staging/$name"
	cp "${build_dirs[$i]}/$target/release/soju" "${build_dirs[$i]}/$target/release/sojuctl" "$staging/$name/"
	cp -r LICENSE README.md config.in doc "$staging/$name/"
	tar -C "$staging" -czf "$dist_dir/$name.tar.gz" "$name"
	archives+=("$name.tar.gz")
done

# Hash only this release's two archives, even if dist contains older builds.
(cd "$dist_dir" && sha256sum "${archives[@]}" > SHA256SUMS)
ls -lh "${archives[@]/#/$dist_dir/}" "$dist_dir/SHA256SUMS"
