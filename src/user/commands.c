// Commands of registered clients: dispatch and BOUNCER (soju-rs
// downstream/cmds/mod.rs, bouncer.rs)
#include "user/cmds.h"

#include "service/service.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"
#include "irc/whox.h"

static void bouncer(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p) {
	const char *sub = irc_msg_param(msg, 0);
	if (cstr_eq(sub, "BIND")) {
		cmd_fail(p, "BOUNCER", "REGISTRATION_IS_COMPLETED", sub,
			 "Cannot bind to a network after registration");
		return;
	}
	if (cstr_eq(sub, "LISTNETWORKS")) {
		user_send_networks(u, p);
		return;
	}
	bool attach = cstr_eq(sub, "ATTACH"), detach = cstr_eq(sub, "DETACH");
	if ((attach || detach) && !cmd_enabled(dc, "soju.im/multiplex")) {
		cmd_fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
		return;
	}
	if (attach || detach) {
		if (dc->network_id != 0 || dc->parent_id != 0) {
			cmd_fail(p, "BOUNCER", "INVALID_CONNECTION", sub,
				 "Networks can only be attached to the bouncer connection");
			return;
		}
	}
	if (cstr_eq(sub, "ADDNETWORK")) {
		struct db_network record;
		db_network_init(&record, "");
		struct irc_tags attrs = IRC_TAGS_INIT;
		irc_tags_parse(&attrs, str_from(irc_msg_param(msg, 1)));
		struct irc_msg *error = nullptr;
		struct err *err = nullptr;
		if (!update_network_attrs(&record, &attrs, sub, &error)) {
			user_packet_add_take(p, error);
		} else if (!user_store_network(u, &record, true, &err)) {
			AUTOFREE char *text =
				xasprintf("Failed to create network: %s", err_msg(err));
			cmd_fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, text);
		} else {
			AUTOFREE char *id = xasprintf("%lld", (long long)record.id);
			user_packet_add_take(p, IRC_MSG("BOUNCER", "ADDNETWORK", id));
		}
		err_free(err);
		irc_tags_free(&attrs);
		db_network_free(&record);
		return;
	}
	if (!attach && !detach && !cstr_eq(sub, "CHANGENETWORK") && !cstr_eq(sub, "DELNETWORK")) {
		cmd_fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
		return;
	}
	long long id;
	if (!str_parse_i64(str_from(irc_msg_param(msg, 1)), &id) || id <= 0) {
		user_packet_add_take(p, IRC_MSG("FAIL", "BOUNCER", "INVALID_NETID", sub,
						irc_msg_param(msg, 1), "Invalid network ID"));
		return;
	}
	struct user_network *net = user_get_network(u, id);
	if (detach) {
		p->detach = true;
		p->network_id = id;
		return;
	}
	if (net == nullptr) {
		user_packet_add_take(p, IRC_MSG("FAIL", "BOUNCER", "INVALID_NETID", sub,
						irc_msg_param(msg, 1), "Invalid network ID"));
		return;
	}
	if (attach) {
		p->attach = true;
		p->network_id = id;
		return;
	}
	struct err *err = nullptr;
	if (cstr_eq(sub, "DELNETWORK")) {
		if (!user_delete_network(u, id, &err)) {
			cmd_fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, err_msg(err));
		} else {
			user_packet_add_take(
				p, IRC_MSG("BOUNCER", "DELNETWORK", irc_msg_param(msg, 1)));
		}
	} else if (cstr_eq(sub, "CHANGENETWORK")) {
		struct db_network record;
		db_network_copy(&record, &net->record);
		struct irc_tags attrs = IRC_TAGS_INIT;
		irc_tags_parse(&attrs, str_from(irc_msg_param(msg, 2)));
		struct irc_msg *error = nullptr;
		if (!update_network_attrs(&record, &attrs, sub, &error)) {
			user_packet_add_take(p, error);
		} else if (!user_store_network(u, &record, true, &err)) {
			AUTOFREE char *text =
				xasprintf("Failed to update network: %s", err_msg(err));
			cmd_fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, text);
		} else {
			user_packet_add_take(
				p, IRC_MSG("BOUNCER", "CHANGENETWORK", irc_msg_param(msg, 1)));
		}
		irc_tags_free(&attrs);
		db_network_free(&record);
	} else {
		cmd_fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
	}
	err_free(err);
}

void user_handle_command(struct user *u, struct user_client *dc, struct irc_msg *msg,
			 uint64_t ack_id) {
	struct user_packet *p = user_packet_new(dc, msg);
	p->ack = true;
	p->ack_id = ack_id;
	struct user_network *net = user_get_network(u, dc->network_id);
	struct upstream *uc = net != nullptr && net->upstream != nullptr && net->upstream->attached
				      ? net->upstream
				      : nullptr;
	const char *cmd = msg->command;
	if (cstr_eq(cmd, "BATCH")) {
		const char *ref = irc_msg_param(msg, 0);
		if (ref[0] == '+' && cstr_eq(irc_msg_param(msg, 1), MULTILINE_BATCH_TYPE) &&
		    cmd_enabled(dc, MULTILINE_BATCH_TYPE)) {
			if (strmap_len(&dc->multiline) < 64 &&
			    !strmap_has(&dc->multiline, ref + 1)) {
				struct multiline_batch *b = xcalloc(1, sizeof(*b));
				multiline_batch_init(b, irc_msg_clone(msg));
				strmap_set(&dc->multiline, ref + 1, b);
			}
			cmd_no_ack(p);
			goto out;
		} else if (ref[0] == '-') {
			struct multiline_batch *b = strmap_take(&dc->multiline, ref + 1);
			if (b == nullptr) {
				goto out;
			}
			irc_msg_free(p->command);
			p->command = irc_msg_clone(b->opener);
			enum multiline_error error;
			struct irc_msg *logical = multiline_batch_finish(b, &error);
			xfree(b);
			if (logical == nullptr) {
				struct multiline_limits limits =
					uc != nullptr
						? multiline_limits_parse(str_from(
							  cap_registry_value(
								  &uc->caps,
								  MULTILINE_BATCH_TYPE) != nullptr
								  ? cap_registry_value(
									    &uc->caps,
									    MULTILINE_BATCH_TYPE)
								  : ""))
						: multiline_limits_default();
				AUTOFREE char *bound = xasprintf("%zu", error == MULTILINE_MAX_LINES
										? limits.max_lines
										: limits.max_bytes);
				user_packet_add_take(p, IRC_MSG("FAIL", "BATCH",
								multiline_error_code(error), bound,
								"Invalid multiline batch"));
				goto out;
			}
			irc_msg_free(msg);
			msg = logical;
			cmd = msg->command;
		}
	} else {
		const char *ref = irc_tags_get(&msg->tags, "batch");
		struct multiline_batch *b =
			ref != nullptr ? strmap_get(&dc->multiline, ref) : nullptr;
		if (b != nullptr) {
			const char *value =
				uc != nullptr ? cap_registry_value(&uc->caps, MULTILINE_BATCH_TYPE)
					      : nullptr;
			struct multiline_limits limits =
				value != nullptr ? multiline_limits_parse(str_from(value))
						 : multiline_limits_default();
			multiline_batch_push(b, msg, &limits);
			cmd_no_ack(p);
			goto out;
		}
	}
	if (cstr_eq(cmd, "PING")) {
		AUTOFREE char *host = server_hostname(u->srv);
		if (msg->params.len > 1 && !cstr_eq(irc_msg_param(msg, 1), host)) {
			user_packet_add_take(p, IRC_MSG("402", dc->nick, irc_msg_param(msg, 1),
							"No such server"));
		} else {
			user_packet_add_take(p, IRC_MSG("PONG", host, irc_msg_param(msg, 0)));
		}
	} else if (cstr_eq(cmd, "PONG")) {
		const char *token = irc_msg_trailing(msg);
		static constexpr char prefix[] = "soju-msgid-";
		if (net != nullptr && str_starts_with(str_from(token), STR_LIT("soju-msgid-"))) {
			int64_t id;
			AUTO_STRBUF struct strbuf target = STRBUF_INIT;
			if (parse_msg_id(str_from(token + sizeof(prefix) - 1), &id, &target,
					 nullptr) &&
			    id == net->record.id &&
			    memchr(target.buf, '\0', target.len) == nullptr) {
				delivered_set(&net->delivered, strbuf_cstr(&target), dc->client,
					      token + sizeof(prefix) - 1);
			}
		}
	} else if (cstr_eq(cmd, "BOUNCER")) {
		bouncer(u, dc, msg, p);
	} else if (cstr_eq(cmd, "METADATA")) {
		user_metadata(u, dc, msg, p);
		irc_msg_free(msg);
		return;
	} else if (cstr_eq(cmd, "CHATHISTORY")) {
		user_history(u, dc, msg, p);
	} else if (cstr_eq(cmd, "SEARCH")) {
		user_search(u, dc, msg, p);
	} else if (cstr_eq(cmd, "MARKREAD") || cstr_eq(cmd, "READ")) {
		user_read_marker(u, dc, msg, p);
	} else if (cstr_eq(cmd, "MONITOR")) {
		cmd_monitor(u, dc, msg, p);
	} else if (cstr_eq(cmd, "WEBPUSH")) {
		if (user_webpush_command(u, dc, msg, p)) {
			cmd_no_ack(p);
		}
	} else if (cstr_eq(cmd, "AWAY")) {
		xfree(dc->away);
		dc->away = msg->params.len > 0 ? xstrdup(irc_msg_param(msg, 0)) : nullptr;
		user_packet_add_take(
			p, IRC_MSG(dc->away != nullptr ? "306" : "305", "*",
				   dc->away != nullptr ? "You have been marked as being away"
						       : "You are no longer marked as being away"));
		if (net != nullptr) {
			user_update_away_monitor(u, net);
		}
	} else if (cstr_eq(cmd, "NICK")) {
		cmd_handle_nick(u, dc, msg, p);
	} else if (cstr_eq(cmd, "SETNAME")) {
		cmd_handle_setname(u, dc, msg, p);
	} else if (cstr_eq(cmd, "JOIN")) {
		cmd_handle_join(u, dc, msg, p);
	} else if (cstr_eq(cmd, "PART")) {
		cmd_handle_part(u, dc, msg, p);
	} else if (cstr_eq(cmd, "MODE")) {
		cmd_handle_mode(u, dc, msg, p);
	} else if (cstr_eq(cmd, "TOPIC")) {
		cmd_handle_topic(u, dc, msg, p);
	} else if (cstr_eq(cmd, "NAMES")) {
		cmd_handle_names(u, dc, msg, p);
	} else if (cstr_eq(cmd, "WHO")) {
		cmd_handle_who(u, dc, msg, p);
	} else if (cstr_eq(cmd, "WHOIS") && cmd_local_query(u, dc, msg, p)) {
		// Answered locally.
	} else if (cstr_eq(cmd, "KICK") || cstr_eq(cmd, "INVITE")) {
		struct upstream *target = cmd_upstream_for(net, p, cmd);
		if (target != nullptr) {
			cmd_send_labeled(target, dc, p, irc_msg_clone(msg));
		}
	} else if ((cstr_eq(cmd, "REGISTER") && msg->params.len < 3) ||
		   (cstr_eq(cmd, "VERIFY") && msg->params.len < 2)) {
		// The parameters are used to save the credentials on success
		user_packet_add_take(p, IRC_MSG("461", "*", cmd, "Not enough parameters"));
	} else if (cstr_eq(cmd, "PRIVMSG") || cstr_eq(cmd, "NOTICE") || cstr_eq(cmd, "TAGMSG")) {
		cmd_privmsg(u, dc, msg, p);
	} else if (cstr_eq(cmd, "CLIENTCERT")) {
		user_clientcert(u, dc, msg, p);
	} else if (cstr_eq(cmd, "AUTHENTICATE") && net != nullptr) {
		cmd_authenticate(dc, net, msg, p);
	} else if (uc != nullptr) {
		if (cstr_eq(cmd, "WHOIS") || cstr_eq(cmd, "LIST") || cstr_eq(cmd, "REGISTER") ||
		    cstr_eq(cmd, "VERIFY")) {
			bool queued = upstream_enqueue_take(uc, dc->id, cmd_take_label(p),
							    irc_msg_clone(msg));
			(void)queued;
		} else if (cstr_eq(cmd, "PASS") || cstr_eq(cmd, "USER")) {
			user_packet_add_take(p, IRC_MSG("462", dc->nick, "You may not reregister"));
			goto out;
		} else {
			cmd_send_labeled(uc, dc, p, irc_msg_clone(msg));
		}
		cmd_no_ack(p);
	} else if (net != nullptr && !cstr_eq(cmd, "PASS") && !cstr_eq(cmd, "USER")) {
		user_packet_add_take(
			p, IRC_MSG("421", "*", cmd, "Disconnected from upstream network"));
	} else if (cstr_eq(cmd, "MOTD")) {
		struct server_config *cfg = server_config_get(u->srv);
		if (cfg->motd[0] != '\0') {
			generate_motd(&p->messages, str_from(cfg->motd));
		} else {
			user_packet_add_take(p, IRC_MSG("422", dc->nick, "No MOTD"));
		}
		server_config_unref(cfg);
	} else if (cstr_eq(cmd, "PASS") || cstr_eq(cmd, "USER")) {
		user_packet_add_take(p, IRC_MSG("462", dc->nick, "You may not reregister"));
	} else if (cstr_eq(cmd, "INFO")) {
		user_packet_add_take(p, IRC_MSG("371", dc->nick, "soju <https://soju.im>"));
		user_packet_add_take(p, IRC_MSG("374", dc->nick, "End of INFO"));
	} else if (cstr_eq(cmd, "LIST")) {
		user_packet_add_take(p, IRC_MSG("323", dc->nick, "End of /LIST"));
	} else if (cstr_eq(cmd, "AUTHENTICATE")) {
		user_packet_add_take(
			p, IRC_MSG("907", dc->nick, "You have already authenticated using SASL"));
	} else {
		user_packet_add_take(p, IRC_MSG("421", "*", cmd, "Unknown command"));
	}
out:
	for (size_t i = 0; i < p->messages.len; i++) {
		// Like soju-rs, BOUNCER and CHATHISTORY TARGETS replies have none
		if (p->messages.data[i]->prefix == nullptr &&
		    !cstr_eq(p->messages.data[i]->command, "BOUNCER") &&
		    !cstr_eq(p->messages.data[i]->command, "CHATHISTORY")) {
			server_set_prefix(u->srv, p->messages.data[i]);
		}
	}
	user_packet_send_take(p);
	irc_msg_free(msg);
}
