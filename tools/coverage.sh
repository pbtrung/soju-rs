#!/bin/bash
# Line coverage of src/ from the unit and integration tests
# (doc/audit-plan.md phase A): builds the clang-coverage preset, runs CTest
# with one profile per process, and writes an llvm-cov HTML report to
# build/coverage/html and a per-file summary to build/coverage/summary.txt.
set -euo pipefail
cd "$(dirname "$0")/.."
out=build/coverage
rm -rf "$out"
mkdir -p "$out/profiles"
cmake --preset clang-coverage >/dev/null
cmake --build --preset clang-coverage
LLVM_PROFILE_FILE="$PWD/$out/profiles/%p-%m.profraw" ctest --preset clang-coverage || true
llvm-profdata merge -sparse "$out"/profiles/*.profraw -o "$out/merged.profdata"
# The first binary is positional, the others are -object
objects=()
for t in build/clang-coverage/test_*; do
	[[ -x "$t" ]] || continue
	if [[ ${#objects[@]} -eq 0 ]]; then
		objects+=("$t")
	else
		objects+=(-object "$t")
	fi
done
llvm-cov report "${objects[@]}" -instr-profile "$out/merged.profdata" src >"$out/summary.txt"
llvm-cov show "${objects[@]}" -instr-profile "$out/merged.profdata" -format=html \
	-output-dir "$out/html" src
tail -1 "$out/summary.txt"
