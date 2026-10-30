// JOIN, PART, MODE, TOPIC and NAMES (soju-rs downstream/cmds/basic.rs)
#include "user/cmds.h"

#include "service/service.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"
#include "irc/whox.h"

void cmd_handle_join(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p) {
	struct user_network *net = user_get_network(u, dc->network_id);
	struct upstream *uc = cmd_upstream_for(net, p, msg->command);
	if (uc == nullptr || !cmd_need_params(msg, 1, p)) {
		return;
	}
	strvec names = {0}, keys = {0};
	cmd_split_list(irc_msg_param(msg, 0), &names);
	if (msg->params.len > 1) {
		cmd_split_list(irc_msg_param(msg, 1), &keys);
	}
	if (names.len > 1) {
		cmd_drop_label(p);
	}
	for (size_t i = 0; i < names.len; i++) {
		const char *name = names.data[i], *key = i < keys.len ? keys.data[i] : "";
		if (name[0] == '\0' || strpbrk(name, ILLEGAL_CHAN_CHARS) != nullptr) {
			user_packet_add_take(p, IRC_MSG(ERR_BADCHANMASK, dc->nick, name,
							"Invalid channel name"));
			continue;
		}
		if (!upstream_is_channel(uc, name)) {
			user_packet_add_take(p, IRC_MSG(ERR_NOSUCHCHANNEL, dc->nick, name,
							"Not a channel name"));
			continue;
		}
		// Clients may JOIN in bulk when reconnecting: don't flood the
		// server with the channels we're in
		if (!cmmap_has(&uc->channels, name)) {
			struct irc_msg *join = IRC_MSG("JOIN", name);
			if (key[0] != '\0') {
				irc_msg_add_param(join, key);
			}
			cmd_send_labeled(uc, dc, p, join);
		}
		struct db_channel *saved = cmmap_get(&net->channels, name);
		struct db_channel record;
		db_channel_init(&record);
		if (saved != nullptr) {
			// Don't clear the channel key if there's one set
			if (key[0] != '\0') {
				db_channel_free(&record);
				record = *saved;
				record.name = xstrdup(saved->name);
				record.key = xstrdup(key);
				record.detached_internal_msg_id =
					xstrdup(saved->detached_internal_msg_id);
				struct err *err = nullptr;
				if (!user_store_channel(u, net, &record, &err)) {
					log_info(&u->srv->log, "failed to update channel: %s",
						 err_msg(err));
					err_free(err);
				}
			}
			user_attach_channel(u, net, name);
		} else {
			xfree(record.name);
			record.name = xstrdup(name);
			xfree(record.key);
			record.key = xstrdup(key);
			struct err *err = nullptr;
			if (!user_store_channel(u, net, &record, &err)) {
				log_info(&u->srv->log, "failed to create channel: %s",
					 err_msg(err));
				err_free(err);
			}
		}
		db_channel_free(&record);
	}
	strvec_free(&names);
	strvec_free(&keys);
}

void cmd_handle_part(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p) {
	struct user_network *net = user_get_network(u, dc->network_id);
	struct upstream *uc = cmd_upstream_for(net, p, msg->command);
	if (uc == nullptr || !cmd_need_params(msg, 1, p)) {
		return;
	}
	const char *reason = msg->params.len > 1 ? irc_msg_param(msg, 1) : "";
	strvec names = {0};
	cmd_split_list(irc_msg_param(msg, 0), &names);
	if (names.len > 1) {
		cmd_drop_label(p);
	}
	for (size_t i = 0; i < names.len; i++) {
		const char *name = names.data[i];
		struct err *err = nullptr;
		if (strcasecmp(reason, "detach") == 0) {
			if (cmmap_has(&net->channels, name)) {
				user_detach_channel(u, net, name);
			} else {
				struct db_channel record;
				db_channel_init(&record);
				xfree(record.name);
				record.name = xstrdup(name);
				record.detached = true;
				if (!user_store_channel(u, net, &record, &err)) {
					log_info(&u->srv->log, "failed to create channel: %s",
						 err_msg(err));
					err_free(err);
				}
				db_channel_free(&record);
			}
		} else {
			struct irc_msg *part = IRC_MSG("PART", name);
			if (reason[0] != '\0') {
				irc_msg_add_param(part, reason);
			}
			cmd_send_labeled(uc, dc, p, part);
			if (!user_delete_channel(u, net, name, &err)) {
				log_info(&u->srv->log, "failed to delete channel: %s",
					 err_msg(err));
				err_free(err);
			}
			cmmap_del(&net->push_targets, name);
		}
	}
	strvec_free(&names);
}

static void send_topic(const struct user_client *dc, struct user_packet *p,
		       const struct upstream_channel *ch) {
	if (ch->topic[0] == '\0') {
		user_packet_add_take(p, IRC_MSG("331", dc->nick, ch->name, "No topic is set"));
		return;
	}
	user_packet_add_take(p, IRC_MSG("332", dc->nick, ch->name, ch->topic));
	if (ch->topic_who[0] != '\0') {
		AUTOFREE char *time =
			xasprintf("%lld", (long long)(ch->topic_time.set ? ch->topic_time.t.sec
									 : -62135596800LL));
		user_packet_add_take(p, IRC_MSG("333", dc->nick, ch->name, ch->topic_who, time));
	}
}

void cmd_handle_mode(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p) {
	if (!cmd_need_params(msg, 1, p)) {
		return;
	}
	const char *name = irc_msg_param(msg, 0);
	const char *modes = msg->params.len > 1 ? irc_msg_param(msg, 1) : "";
	struct user_network *net = user_get_network(u, dc->network_id);
	if (cmd_same_name(cmd_client_casemap(net), name, dc->nick)) {
		if (modes[0] != '\0') {
			struct upstream *uc = cmd_upstream_for(net, p, msg->command);
			if (uc != nullptr) {
				cmd_send_labeled(uc, dc, p, irc_msg_clone(msg));
			}
		} else {
			struct upstream *uc = net != nullptr ? net->upstream : nullptr;
			AUTOFREE char *reply = xasprintf("+%s", uc != nullptr && uc->attached
									? strbuf_cstr(&uc->modes.s)
									: "");
			user_packet_add_take(p, IRC_MSG("221", dc->nick, reply));
		}
		return;
	}
	struct upstream *uc = cmd_upstream_for(net, p, msg->command);
	if (uc == nullptr) {
		return;
	}
	if (!upstream_is_channel(uc, name)) {
		user_packet_add_take(p, IRC_MSG(ERR_USERSDONTMATCH, dc->nick,
						"Cannot change mode for other users"));
		return;
	}
	if (modes[0] != '\0') {
		struct irc_msg *mode = irc_msg_clone(msg);
		irc_msg_clear_prefix(mode);
		irc_tags_clear(&mode->tags);
		cmd_send_labeled(uc, dc, p, mode);
		return;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, name);
	if (ch == nullptr) {
		// We're not on that channel
		cmd_send_labeled(uc, dc, p, IRC_MSG("MODE", name));
		return;
	}
	if (!ch->has_modes) {
		// The modes are broadcast when RPL_CHANNELMODEIS arrives
		return;
	}
	AUTO_STRBUF struct strbuf mode_str = STRBUF_INIT;
	strvec params = {0};
	format_channel_modes(&ch->modes, &mode_str, &params);
	struct irc_msg *reply = IRC_MSG("324", dc->nick, ch->name, strbuf_cstr(&mode_str));
	for (size_t i = 0; i < params.len; i++) {
		irc_msg_add_param(reply, params.data[i]);
	}
	strvec_free(&params);
	user_packet_add_take(p, reply);
	if (ch->creation_time[0] != '\0') {
		user_packet_add_take(p, IRC_MSG("329", dc->nick, ch->name, ch->creation_time));
	}
}

void cmd_handle_topic(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		      struct user_packet *p) {
	if (!cmd_need_params(msg, 1, p)) {
		return;
	}
	struct upstream *uc =
		cmd_upstream_for(user_get_network(u, dc->network_id), p, msg->command);
	if (uc == nullptr) {
		return;
	}
	const char *name = irc_msg_param(msg, 0);
	struct upstream_channel *ch = cmmap_get(&uc->channels, name);
	if (msg->params.len > 1) {
		cmd_send_labeled(uc, dc, p, IRC_MSG("TOPIC", name, irc_msg_param(msg, 1)));
	} else if (ch == nullptr) {
		cmd_send_labeled(uc, dc, p, IRC_MSG("TOPIC", name));
	} else {
		send_topic(dc, p, ch);
	}
}

void cmd_handle_names(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		      struct user_packet *p) {
	struct upstream *uc =
		cmd_upstream_for(user_get_network(u, dc->network_id), p, msg->command);
	if (uc == nullptr) {
		return;
	}
	if (msg->params.len == 0) {
		user_packet_add_take(p, IRC_MSG("366", dc->nick, "*", "End of /NAMES list"));
		return;
	}
	strvec names = {0};
	cmd_split_list(irc_msg_param(msg, 0), &names);
	if (names.len > 1) {
		cmd_drop_label(p);
	}
	for (size_t i = 0; i < names.len; i++) {
		struct upstream_channel *ch = cmmap_get(&uc->channels, names.data[i]);
		if (ch != nullptr) {
			user_names_reply(dc, p, ch);
		} else {
			cmd_send_labeled(uc, dc, p, IRC_MSG("NAMES", names.data[i]));
		}
	}
	strvec_free(&names);
}
