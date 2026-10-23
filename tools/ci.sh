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
	jobs="${2:-}"
	[[ "$jobs" =~ ^[1-9][0-9]*$ ]] || { echo 'usage: tools/ci.sh [-j N] [preset...]' >&2; exit 2; }
	shift 2
fi
export CMAKE_BUILD_PARALLEL_LEVEL="${CMAKE_BUILD_PARALLEL_LEVEL:-4}"

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
	local p="$1" log="build/ci-$1.log" lock
	mkdir -p build
	# Serialize repeated invocations for the same build tree and log.
	exec {lock}>"build/ci-$p.lock"
	flock "$lock"
	if (
		echo "== $p"
		cmake --preset "$p" || exit 1
		if [[ -z "${CI_CONFIGURE_ONLY:-}" ]]; then
			cmake --build --preset "$p" || exit 1
			ctest --preset "$p" --output-junit "ci-$p.xml" || exit 1
		fi
	) >"$log" 2>&1; then
		echo "ok     $p"
	else
		echo "FAILED $p (see $log)"
		return 1
	fi
}
export -f run_preset

printf '%s\n' "${presets[@]}" | xargs -P "$jobs" -I{} bash -c "run_preset \"\$1\"" _ '{}'
