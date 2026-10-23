// Errors: an owned message plus an optional code, like anyhow::Error with
// a downcastable kind. Functions that can fail return bool (true on
// success) and take a struct err **err; on failure they set *err to a new
// error which the caller owns:
//
//   struct err *e = nullptr;
//   if (!parse(s, &x, &e)) { log("%s", err_msg(e)); err_free(e); }
//
// err may be nullptr when the caller doesn't care about the reason.
#pragma once

#include <stdarg.h>

struct err {
	int code; // 0 unless the creator gives one (module-specific values)
	char *msg;
};

// Sets *err (if err isn't nullptr) and returns false, so failing code can
// `return err_set(err, 0, "bad %s", x);`
[[gnu::format(printf, 3, 4)]] bool err_set(struct err **err, int code, const char *fmt, ...);
// Prefixes the message of an existing error: "context: message"
[[gnu::format(printf, 2, 3)]] bool err_wrap(struct err **err, const char *fmt, ...);
[[nodiscard]] const char *err_msg(const struct err *e);
void err_free(struct err *e);
