# Coding conventions

Rules for all C code in this tree. They implement PLAN.md section 2.1;
read it first.

## Porting

- The behaviour reference is soju-rs on the `main` branch: read the Rust
  source with `git show main:src/<path>.rs`. Port behaviour exactly,
  including error messages, edge cases and odd corners; when C forces a
  difference (e.g. NUL bytes can't appear in C strings), document it in a
  comment and in the module's header.
- One C module per Rust module where practical, at the path given in
  PLAN.md section 4. A module is `src/<dir>/<name>.h` + `.c`.
- Every Rust `#[test]` is ported to a cmocka test with the same name
  (`fn parse_basic` → `test_parse_basic`) in `tests/unit/test_<dir>_<name>.c`,
  starting with a comment naming the Rust file it's ported from. Add C-only
  tests for C-only concerns (ownership, error paths, limits). List every
  ported test in `tests/PORTED_TESTS.md`.

## Language and build

- C23 (`nullptr`, `bool`, `constexpr`, `[[nodiscard]]`, `static_assert`,
  `typeof`, `<stdckdint.h>`). GNU extensions only through standard attribute
  syntax (`[[gnu::cleanup]]`, `[[gnu::format]]`).
- Build with presets: `cmake --preset gcc-debug && cmake --build --preset
  gcc-debug && ctest --preset gcc-debug`; also `gcc-asan`, `clang-asan`,
  `clang-msan`, `clang-tsan`, `gcc-ubsan-trap`, `clang-fuzz`. Warnings are
  errors. New `src/**/*.c`, `tests/unit/test_*.c`, `fuzz/fuzz_*.c` and
  `spikes/spike_*.c` files are picked up automatically (re-run the
  configure step).
- Format with `clang-format -i` (`.clang-format`): tabs, 100 columns.

## Memory

- Allocate with `xmalloc`/`xcalloc`/`xrealloc`/`xstrdup`/`xasprintf`
  (`base/alloc.h`), free with `xfree`. They abort on OOM: never check for
  nullptr. Size arithmetic feeding an allocation or memcpy uses
  `size_add`/`size_mul` or `ckd_add`/`ckd_mul`.
- Free library allocations with the library's function (utf8proc's with
  `free`, yyjson's with `yyjson_doc_free`, ...), never with `xfree`; in the
  debug build of the tests, xmalloc goes through cmocka's checked allocator,
  so mixing them crashes.
- Strings: `struct str` (borrowed slice, not NUL-terminated) and
  `struct strbuf` (owned, growable) from `base/str.h`. No `strcpy`, `strcat`,
  `sprintf`, `strtok`, `atoi`, VLAs or `alloca`.
- Containers: `VEC(T)`/`strvec` (`base/vec.h`), `struct strmap` (ordered
  string map, `base/map.h`), `struct arena` (`base/arena.h`).
- Ownership: a function returning a pointer gives the caller ownership
  unless its name starts with `get`/`peek` or the comment says it's
  borrowed. Functions that take ownership of an argument say so. Every
  `*_free(nullptr)` is a no-op. Embedded structs have `x_init`/`x_finish`
  (or `x_free` freeing members only); heap objects have `x_new`/`x_free`.
- Errors: functions that can fail return `bool` and take
  `struct err **err` (`base/err.h`); `return err_set(err, code, "...")`.
- Long-lived objects embed `struct ref_node` and call `REF_TRACK`
  (`base/ref.h`); tests fail if any is left alive.
- Locals that own memory may use `AUTOFREE` / `AUTO_STRBUF`; otherwise use
  a single `goto out;` cleanup path.

## Tests and fuzzing

- `tests/support/testsupport.h`: `T_MAIN(cmocka_unit_test(test_x), ...)`,
  `assert_str_eq(struct str, "cstr")`, `assert_cstr_eq`.
- Each parser gets a libFuzzer harness `fuzz/fuzz_<name>.c` defining
  `LLVMFuzzerTestOneInput`, with seeds in `fuzz/seeds/fuzz_<name>/` (small
  files, committed). Outside the `fuzz` preset the harness is linked with a
  replay driver and its seeds run as a CTest test. Check properties
  (round trips, output validity), not just "doesn't crash"; call `abort()`
  on a violated property. Run `build/clang-fuzz/fuzz_<name> -max_total_time=60
  fuzz/seeds/fuzz_<name>` (copy the seeds to a scratch directory first if
  you don't want new corpus files added to the seeds directory).
