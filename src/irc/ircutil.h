// IRC helpers used by the bouncer logic (soju-rs ircutil.rs, a port of
// upstream's irc.go)
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "base/err.h"
#include "base/timeutil.h"
#include "irc/casemapping.h"
#include "irc/message.h"
#include "irc/xirc.h"

// A message for logs, hiding the parameters of commands carrying
// credentials (a new string)
[[nodiscard]] char *redact_for_log(const struct irc_msg *msg);

// A set of user modes (mode letters, in order of addition)
struct user_modes {
	struct strbuf s;
};

[[nodiscard]] bool user_modes_has(const struct user_modes *m, uint8_t c);
void user_modes_add(struct user_modes *m, uint8_t c);
void user_modes_del(struct user_modes *m, uint8_t c);
[[nodiscard]] bool user_modes_apply(struct user_modes *m, struct str s, struct err **err);
void user_modes_free(struct user_modes *m);

// Standard channel mode types (https://modern.ircdocs.horse/#mode-message)
enum channel_mode_type {
	CHANMODE_A, // add or remove an address to or from a list
	CHANMODE_B, // a setting which must always have a parameter
	CHANMODE_C, // a setting with a parameter when set, none when unset
	CHANMODE_D, // a setting which must not have a parameter
};

struct channel_mode_type_entry {
	uint8_t mode;
	enum channel_mode_type type;
};

// Ordered mode letter -> type
struct channel_mode_types {
	VEC(struct channel_mode_type_entry) v;
};

void std_channel_modes(struct channel_mode_types *out);
// Sets (or replaces in place) a mode's type
void channel_mode_types_set(struct channel_mode_types *t, uint8_t mode,
			    enum channel_mode_type type);
[[nodiscard]] bool channel_mode_types_get(const struct channel_mode_types *t, uint8_t mode,
					  enum channel_mode_type *out);
void channel_mode_types_free(struct channel_mode_types *t);

// Channel modes: mode letter -> value ("" for none), in insertion order
// (IndexMap<u8, String>)
struct channel_mode {
	uint8_t mode;
	char *value;
};

struct channel_modes {
	VEC(struct channel_mode) v;
};

// Inserts or replaces in place
void channel_modes_set(struct channel_modes *cm, uint8_t mode, const char *value);
// Removes, keeping the order of the others
void channel_modes_remove(struct channel_modes *cm, uint8_t mode);
// The value, or nullptr
[[nodiscard]] const char *channel_modes_get(const struct channel_modes *cm, uint8_t mode);
void channel_modes_free(struct channel_modes *cm);

static constexpr char STD_CHANNEL_TYPES[] = "#&+!";
// founder ~, protected &, operator @, halfop %, voice +
[[nodiscard]] const struct membership *std_memberships(size_t *n);

// Applies the channel mode and membership changes of a MODE message (or
// RPL_CHANNELMODEIS). members maps nicks to struct membership_set *; modes
// may be nullptr to leave channel modes alone.
[[nodiscard]] bool apply_channel_modes(struct channel_modes *modes, struct cmmap *members,
				       const struct membership *available, size_t n_available,
				       const struct channel_mode_types *mode_types,
				       struct str mode_str, const char *const *args, size_t n_args,
				       struct err **err);
// The mode string ("+kln") and its parameters
void format_channel_modes(const struct channel_modes *cm, struct strbuf *mode_str, strvec *params);
void format_member_prefix(struct strbuf *out, const struct membership_set *ms, bool multi_prefix);
// Removes channel membership prefixes from flags
void strip_member_prefixes(struct strbuf *out, struct str flags, const struct membership *ms,
			   size_t n);
// The client tags ("+..."), appended to out
void copy_client_tags(struct irc_tags *out, const struct irc_tags *tags);
// nick must not be empty
[[nodiscard]] bool is_highlight(struct str text, struct str nick);
// A CHATHISTORY bound: "timestamp=<server time>"
[[nodiscard]] bool parse_chathistory_bound(struct str param, struct timestamp *out);
[[nodiscard]] bool is_numeric(const char *cmd);
[[nodiscard]] struct irc_msg *generate_away_reply(bool away);
// Splits a service command line into words, following POSIX shell quoting
// rules (upstream's splitWords)
[[nodiscard]] bool split_words(struct str s, strvec *words, struct err **err);
