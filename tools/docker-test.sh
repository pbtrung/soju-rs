#!/bin/bash
# Build and test natively with fresh GNU or musl toolchains.
set -euo pipefail
cd "$(dirname "$0")/.."
target="${1:-}"
[[ "$target" == gnu || "$target" == musl ]] || { echo 'usage: tools/docker-test.sh <gnu|musl> [preset...]' >&2; exit 2; }
shift
image="soju-build-$target"
if [[ "${SOJU_DOCKER_REUSE:-0}" != 1 ]]; then
	docker build --pull --no-cache -t "$image" -f "docker/Dockerfile.$target" .
fi
mkdir -p "build/docker/$target"
docker run --rm --init --security-opt seccomp=unconfined \
	--user "$(id -u):$(id -g)" \
	-v "$PWD:/src:ro" -v "$PWD/build/docker/$target:/src/build" \
	-w /src "$image" bash -c \
	'gcc --version; clang --version; ldd --version 2>&1 || true; exec tools/ci.sh "$@"' bash "$@"
