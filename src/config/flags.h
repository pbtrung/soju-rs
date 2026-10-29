// Command-line flag parsing with the same syntax and error messages as Go's
// flag package, used by BouncerServ commands (soju-rs flags.rs)
#pragma once

#include "base/err.h"
#include "base/map.h"
#include "base/str.h"
#include "base/vec.h"

enum flag_kind {
	FLAG_BOOL, // may be given without a value (-flag)
	FLAG_BOOL_VALUE, // requires a value (-flag true)
	FLAG_STRING,
	FLAG_INT,
	FLAG_ATOI, // an integer parsed like strconv.Atoi (custom Go flag values)
};

struct flag_set {
	struct strmap flags; // name -> (enum flag_kind + 1)
};

void flag_set_init(struct flag_set *fs);
void flag_set_free(struct flag_set *fs);
void flag_set_add(struct flag_set *fs, const char *name, enum flag_kind kind);

// Parse results: flag values in order of appearance, and the remaining
// positional arguments
struct flags_parsed {
	struct strmap values; // name -> strvec *
	strvec args;
};

void flags_parsed_free(struct flags_parsed *p);
// The last value of a flag, or nullptr
[[nodiscard]] const char *flags_get(const struct flags_parsed *p, const char *name);
// All values of a repeatable flag, or nullptr
[[nodiscard]] const strvec *flags_get_all(const struct flags_parsed *p, const char *name);
// false if missing or not a boolean
[[nodiscard]] bool flags_get_bool(const struct flags_parsed *p, const char *name, bool *out);
[[nodiscard]] bool flags_get_int(const struct flags_parsed *p, const char *name, long long *out);

// err code for -h/-help: the message is "flag: help requested"
static constexpr int FLAG_ERR_HELP = 1;
[[nodiscard]] bool flag_set_parse(const struct flag_set *fs, const char *const *args, size_t n,
				  struct flags_parsed *out, struct err **err);

// Go's strconv.ParseBool
[[nodiscard]] bool parse_go_bool(struct str s, bool *out);
