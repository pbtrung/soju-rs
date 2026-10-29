// WHOX replies (soju-rs xirc/whox.rs)
#pragma once

#include "base/err.h"
#include "irc/message.h"

// All strings are owned; whox_info_free frees them
struct whox_info {
	char *token, *channel, *username, *hostname, *server, *nickname, *flags, *account,
		*realname;
};

// Initializes every field to ""
void whox_info_init(struct whox_info *info);
void whox_info_free(struct whox_info *info);
// Replaces field k (a WHOX field letter); unknown letters are ignored
void whox_info_set(struct whox_info *info, char k, const char *v);

[[nodiscard]] struct irc_msg *generate_whox_reply(const char *fields, const struct whox_info *info);
// Parses WHO options: *fields and *token are new strings
void parse_whox_options(struct str options, char **fields, char **token);
[[nodiscard]] bool parse_whox_reply(const struct irc_msg *msg, const char *fields,
				    struct whox_info *info, struct err **err);
