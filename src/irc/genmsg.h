// Generators of messages split to fit IRC limits (soju-rs xirc/genmsg.rs)
#pragma once

#include <stddef.h>

#include "irc/message.h"
#include "irc/xirc.h"

// JOIN messages for channels with their keys ("" for none), split to fit
// the maximum message length; channels with a key come first
void generate_join(irc_msg_vec *out, const char *const *channels, const char *const *keys,
		   size_t n);
// RPL_ISUPPORT messages, tokens sorted
void generate_isupport(irc_msg_vec *out, const struct isupport *is);
void generate_motd(irc_msg_vec *out, struct str motd);
void generate_monitor(irc_msg_vec *out, const char *subcmd, const char *const *targets, size_t n);
void generate_names_reply(irc_msg_vec *out, const char *channel, enum channel_status status,
			  const char *const *members, size_t n);
// AUTHENTICATE messages for a SASL response, split in chunks
void generate_sasl(irc_msg_vec *out, const void *resp, size_t len);
