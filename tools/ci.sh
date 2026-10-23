#!/bin/bash
# Configures, builds and tests CMake presets (default: every preset of this
# compiler set that runs on this machine), PLAN.md 2.5.
#
# Usage: tools/ci.sh [-j N] [preset...]
#   tools/ci.sh                    all presets
#   tools/ci.sh gcc-asan clang-msan
#   CI_CONFIGURE_ONLY=1 tools/ci.sh   only configure (builds dependencies)
set -euo pipefail
cd "$(dirname "$0")/.."

jobs=1
if [[ "${1:-}" == "-j" ]]; then
	jobs="$2"
	shift 2
fi

presets=("$@")
if [[ ${#presets[@]} -eq 0 ]]; then
	mapfile -t presets < <(cmake --list-presets=configure | sed -n 's/^  "\([^"]*\)".*/\1/p')
	if ! command -v valgrind >/dev/null; then
		echo "valgrind not installed: skipping valgrind presets" >&2
		mapfile -t presets < <(printf '%s\n' "${presets[@]}" | grep -v valgrind)
	fi
	if ldd --version 2>&1 | grep -qi musl; then
		mapfile -t presets < <(printf '%s\n' "${presets[@]}" | grep -Ev 'asan|msan|tsan|fuzz')
	fi
fi

run_preset() {
	local p="$1" log="build/ci-$1.log"
	mkdir -p build
	{
		echo "== $p"
		cmake --preset "$p"
		if [[ -z "${CI_CONFIGURE_ONLY:-}" ]]; then
			cmake --build --preset "$p"
			ctest --preset "$p"
		fi
	} >"$log" 2>&1 && echo "ok     $p" || { echo "FAILED $p (see $log)"; return 1; }
}
export -f run_preset

printf '%s\n' "${presets[@]}" | xargs -P "$jobs" -I{} bash -c 'run_preset {}'
