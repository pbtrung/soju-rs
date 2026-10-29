// The C side of the differential oracle (tests/oracle): runs an op on the
// C port and formats the result exactly like tests/oracle/src/main.rs.
#pragma once

#include <stddef.h>

#include "base/str.h"

// The op names, nullptr-terminated
extern const char *const oracle_ops[];
// Appends the result of op on input to out
void oracle_run_c(const char *op, struct str input, struct strbuf *out);

// A running oracle process (SOJU_ORACLE, or build/oracle/release/soju-oracle)
struct oracle;
// nullptr if the oracle isn't available
[[nodiscard]] struct oracle *oracle_start(void);
void oracle_stop(struct oracle *o);
// Appends the result of the Rust code to out
void oracle_run_rust(struct oracle *o, const char *op, struct str input, struct strbuf *out);
