// Differential fuzzing against soju-rs (PLAN.md 2.3 L5): the first byte of
// the input selects an op of tests/oracle, the rest is its input; the C
// port must produce the same output. Without the oracle binary, inputs
// are only run through the C code.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include "base/str.h"
#include "oracle_ops.h"

// NOLINTNEXTLINE(misc-use-internal-linkage): called by libFuzzer or fuzz/replay.c
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

static struct oracle *oracle;
static bool started;

static void stop(void) {
	oracle_stop(oracle);
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
	if (!started) {
		started = true;
		oracle = oracle_start();
		atexit(stop);
	}
	if (size == 0) {
		return 0;
	}
	size_t n_ops = 1;
	while (oracle_ops[n_ops] != nullptr) {
		n_ops++;
	}
	const char *op = oracle_ops[data[0] % n_ops];
	struct str input = str_new((const char *)data + 1, size - 1);
	// The C port works on NUL-terminated strings: text with NUL bytes
	// isn't comparable (b64 ops take raw bytes and handle them)
	if (memchr(input.p, '\0', input.len) != nullptr && !cstr_eq(op, "pathmatch") &&
	    !cstr_eq(op, "highlight") && !cstr_eq(op, "b64std") && !cstr_eq(op, "b64url")) {
		return 0;
	}
	// Two NULs would be a third field the ops don't have
	struct strbuf c = STRBUF_INIT;
	oracle_run_c(op, input, &c);
	if (oracle != nullptr) {
		struct strbuf r = STRBUF_INIT;
		oracle_run_rust(oracle, op, input, &r);
		if (!str_eq(strbuf_str(&c), strbuf_str(&r))) {
			fprintf(stderr, "op %s: C and Rust differ\nC:    %s\nRust: %s\n", op,
				strbuf_cstr(&c), strbuf_cstr(&r));
			abort();
		}
		strbuf_free(&r);
	}
	strbuf_free(&c);
	return 0;
}
