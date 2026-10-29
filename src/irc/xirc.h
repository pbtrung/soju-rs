// Helpers shared by upstream and downstream code (soju-rs xirc/mod.rs).
// Server times are in base/timeutil.h.
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "base/err.h"
#include "base/map.h"
#include "base/str.h"
#include "base/vec.h"

static constexpr size_t MAX_MESSAGE_LENGTH = 512;
static constexpr size_t MAX_MESSAGE_PARAMS = 15;
static constexpr size_t MAX_SASL_LENGTH = 400;

// A set of capabilities: the available ones (name -> value) and the enabled
// ones, both in insertion order
struct cap_registry {
	struct strmap available; // values: owned strings
	struct strmap enabled; // values unused
};

void cap_registry_init(struct cap_registry *r);
void cap_registry_free(struct cap_registry *r);
[[nodiscard]] bool cap_registry_is_available(const struct cap_registry *r, const char *name);
[[nodiscard]] bool cap_registry_is_enabled(const struct cap_registry *r, const char *name);
// The value of an available capability, or nullptr
[[nodiscard]] const char *cap_registry_value(const struct cap_registry *r, const char *name);
void cap_registry_set_available(struct cap_registry *r, const char *name, const char *value);
void cap_registry_del(struct cap_registry *r, const char *name);
void cap_registry_set_enabled(struct cap_registry *r, const char *name, bool enabled);

enum channel_status {
	CHANNEL_PUBLIC,
	CHANNEL_SECRET,
	CHANNEL_PRIVATE,
};

// "=", "@" or "*"
[[nodiscard]] bool channel_status_parse(struct str s, enum channel_status *out, struct err **err);
[[nodiscard]] const char *channel_status_str(enum channel_status s);

// A channel member rank
struct membership {
	uint8_t mode;
	uint8_t prefix;
};

static inline bool membership_eq(struct membership a, struct membership b) {
	return a.mode == b.mode && a.prefix == b.prefix;
}

// Memberships sorted by descending rank
struct membership_set {
	VEC(struct membership) v;
};

void membership_set_add(struct membership_set *ms, const struct membership *available,
			size_t n_available, struct membership m);
void membership_set_remove(struct membership_set *ms, struct membership m);
void membership_set_free(struct membership_set *ms);

void decode_isupport_value(struct strbuf *out, struct str v);
void encode_isupport_value(struct strbuf *out, struct str v);

struct replace_pair {
	const char *old;
	const char *new;
};

// Replaces substrings left-to-right like Go's strings.Replacer: at each
// position, the first matching pattern wins and the output isn't rescanned
void replace_all(struct strbuf *out, struct str s, const struct replace_pair *pairs, size_t n);

// ISUPPORT tokens: name -> value (owned string, or nullptr for a token
// without a value)
struct isupport {
	struct strmap m;
};

void isupport_init(struct isupport *is);
void isupport_free(struct isupport *is);
// value may be nullptr
void isupport_set(struct isupport *is, const char *name, const char *value);
