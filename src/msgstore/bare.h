// Minimal BARE (https://baremessages.org) encoding, as used by upstream for
// internal message IDs (soju-rs msgstore/bare.rs)
#pragma once

#include <stdint.h>

#include "base/err.h"
#include "base/str.h"

// Writers append to a byte buffer
void bare_write_uint(struct strbuf *w, uint64_t v);
void bare_write_int(struct strbuf *w, int64_t v);
void bare_write_string(struct strbuf *w, struct str s);

// A reader consumes r
[[nodiscard]] bool bare_read_uint(struct str *r, uint64_t *out, struct err **err);
[[nodiscard]] bool bare_read_int(struct str *r, int64_t *out, struct err **err);
// Go doesn't validate UTF-8: the string is decoded lossily (appended to out)
[[nodiscard]] bool bare_read_string(struct str *r, struct strbuf *out, struct err **err);
