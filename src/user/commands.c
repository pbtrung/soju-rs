#include "user/internal.h"
#include "service/service.h"

#include "base/alloc.h"
#include "irc/genmsg.h"
#include "irc/whox.h"

static void fail(struct user_packet *p, const char *cmd, const char *code, const char *sub,
		 const char *text) {
	user_packet_add_take(p, IRC_MSG("FAIL", cmd, code, sub, text));
}
static bool enabled(const struct user_client *dc, const char *cap) {
	return cap_registry_is_enabled(&dc->caps, cap);
}
static void no_ack(struct user_packet *p) {
	irc_msg_free(p->command);
	p->command = nullptr;
}

static void bouncer(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p) {
	const char *sub = irc_msg_param(msg, 0);
	if (cstr_eq(sub, "BIND")) {
		fail(p, "BOUNCER", "REGISTRATION_IS_COMPLETED", sub,
		     "Cannot bind to a network after registration");
		return;
	}
	if (cstr_eq(sub, "LISTNETWORKS")) {
		user_send_networks(u, p);
		return;
	}
	bool attach = cstr_eq(sub, "ATTACH"), detach = cstr_eq(sub, "DETACH");
	if ((attach || detach) && !enabled(dc, "soju.im/multiplex")) {
		fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
		return;
	}
	if (attach || detach) {
		if (dc->network_id != 0 || dc->parent_id != 0) {
			fail(p, "BOUNCER", "INVALID_CONNECTION", sub,
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
			fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, text);
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
		fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
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
			fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, err_msg(err));
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
			fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, text);
		} else {
			user_packet_add_take(
				p, IRC_MSG("BOUNCER", "CHANGENETWORK", irc_msg_param(msg, 1)));
		}
		irc_tags_free(&attrs);
		db_network_free(&record);
	} else {
		fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
	}
	err_free(err);
}

static void monitor(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p) {
	const char *sub = irc_msg_param(msg, 0);
	struct user_network *net = user_get_network(u, dc->network_id);
	if (cstr_eq(sub, "C")) {
		cmmap_clear(&dc->monitored);
	} else if (cstr_eq(sub, "+") || cstr_eq(sub, "-")) {
		struct str rest = str_from(irc_msg_param(msg, 1)), part;
		if (str_contains(rest, ' ') || str_contains(rest, ':')) {
			return;
		}
		while (str_next_field(&rest, ',', &part)) {
			if (part.len == 0) {
				continue;
			}
			AUTOFREE char *name = str_dup(part);
			if (cstr_eq(sub, "-")) {
				cmmap_del(&dc->monitored, name);
			} else if (cmmap_len(&dc->monitored) < 1000) {
				cmmap_set(&dc->monitored, name, nullptr);
			}
		}
	}
	if (cstr_eq(sub, "L")) {
		const char *name;
		for (size_t it = 0; cmmap_next(&dc->monitored, &it, &name, nullptr);) {
			user_packet_add_take(p, IRC_MSG("732", dc->nick, name));
		}
		user_packet_add_take(p, IRC_MSG("733", dc->nick, "End of MONITOR list"));
	} else if (cstr_eq(sub, "S") || cstr_eq(sub, "+")) {
		const char *name;
		for (size_t it = 0; cmmap_next(&dc->monitored, &it, &name, nullptr);) {
			AUTOFREE char *canonical = casemap_cstr(CASEMAPPING_ASCII, name);
			bool *up = net != nullptr && net->upstream != nullptr
					   ? cmmap_get(&net->upstream->monitored, name)
					   : nullptr;
			if (up != nullptr || cstr_eq(canonical, SERVICE_NICK_CM) ||
			    net == nullptr) {
				bool online = cstr_eq(canonical, SERVICE_NICK_CM) ||
					      (up != nullptr && *up);
				user_packet_add_take(
					p, IRC_MSG(online ? "730" : "731", dc->nick, name));
			}
		}
	}
	if (net != nullptr) {
		user_update_away_monitor(u, net);
	}
}

static bool local_query(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p) {
	const char *mask =
		cstr_eq(msg->command, "WHO") ? irc_msg_param(msg, 0) : irc_msg_trailing(msg);
	AUTOFREE char *cm = casemap_cstr(CASEMAPPING_ASCII, mask);
	bool service = cstr_eq(cm, SERVICE_NICK_CM);
	if (!service && dc->network_id != 0) {
		return false;
	}
	bool self = cstr_eq(mask, dc->nick);
	AUTOFREE char *host = server_hostname(u->srv);
	if (cstr_eq(msg->command, "WHOIS")) {
		if (service || self) {
			user_packet_add_take(p, IRC_MSG("311", dc->nick, mask,
							service ? SERVICE_NICK : u->record.username,
							service ? SERVICE_NICK : dc->hostname, "*",
							service ? SERVICE_REALNAME : dc->realname));
			user_packet_add_take(p, IRC_MSG("312", dc->nick, mask, host, "soju"));
			if (service || u->record.admin) {
				user_packet_add_take(
					p, IRC_MSG("313", dc->nick, mask,
						   service ? "is the bouncer service"
							   : "is a bouncer administrator"));
			}
			user_packet_add_take(p, IRC_MSG("330", dc->nick, mask,
							service ? SERVICE_NICK : u->record.username,
							"is logged in as"));
			if (service) {
				user_packet_add_take(p, IRC_MSG("335", dc->nick, mask, "is a bot"));
			}
		} else {
			user_packet_add_take(
				p, IRC_MSG("401", dc->nick, mask, "No such nick/channel"));
		}
		user_packet_add_take(p, IRC_MSG("318", dc->nick, mask, "End of /WHOIS list"));
	} else if (cstr_eq(msg->command, "WHO")) {
		if (service || self) {
			char *fields, *token;
			parse_whox_options(str_from(irc_msg_param(msg, 1)), &fields, &token);
			struct whox_info info;
			whox_info_init(&info);
			whox_info_set(&info, 't', token);
			whox_info_set(&info, 'n', service ? SERVICE_NICK : dc->nick);
			whox_info_set(&info, 'u', service ? SERVICE_NICK : u->record.username);
			whox_info_set(&info, 'h', service ? SERVICE_NICK : dc->hostname);
			whox_info_set(&info, 's', host);
			whox_info_set(&info, 'f',
				      service               ? "H*B"
				      : u->record.admin     ? "H*"
				      : dc->away != nullptr ? "G"
							    : "H");
			whox_info_set(&info, 'a', service ? SERVICE_NICK : u->record.username);
			whox_info_set(&info, 'r', service ? SERVICE_REALNAME : dc->realname);
			user_packet_add_take(p, generate_whox_reply(fields, &info));
			whox_info_free(&info);
			xfree(fields);
			xfree(token);
		}
		user_packet_add_take(
			p, IRC_MSG("315", "*", irc_msg_param(msg, 0), "End of /WHO list"));
	} else {
		return false;
	}
	return true;
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
		    enabled(dc, MULTILINE_BATCH_TYPE)) {
			if (strmap_len(&dc->multiline) < 64 &&
			    !strmap_has(&dc->multiline, ref + 1)) {
				struct multiline_batch *b = xcalloc(1, sizeof(*b));
				multiline_batch_init(b, irc_msg_clone(msg));
				strmap_set(&dc->multiline, ref + 1, b);
			}
			no_ack(p);
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
			no_ack(p);
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
		monitor(u, dc, msg, p);
	} else if (cstr_eq(cmd, "WEBPUSH")) {
		if (user_webpush_command(u, dc, msg, p)) {
			no_ack(p);
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
	} else if ((cstr_eq(cmd, "WHO") || cstr_eq(cmd, "WHOIS")) && local_query(u, dc, msg, p)) {
		// Answered locally.
	} else if (cstr_eq(cmd, "PRIVMSG") || cstr_eq(cmd, "NOTICE") || cstr_eq(cmd, "TAGMSG")) {
		if (msg->params.len < (cstr_eq(cmd, "TAGMSG") ? 1U : 2U)) {
			user_packet_add_take(
				p, IRC_MSG("461", "*", cmd, "Not enough parameters"));
			goto out;
		}
		AUTOFREE char *canonical = casemap_cstr(CASEMAPPING_ASCII, irc_msg_param(msg, 0));
		if (cstr_eq(canonical, SERVICE_NICK_CM)) {
			if (enabled(dc, "echo-message")) {
				struct irc_msg *echo = irc_msg_clone(msg);
				irc_tags_clear(&echo->tags);
				copy_client_tags(&echo->tags, &msg->tags);
				AUTOFREE char *now = format_server_time_cstr(timestamp_now());
				irc_tags_set(&echo->tags, "time", now);
				irc_msg_set_prefix(echo, DOWNSTREAM_SELF_PREFIX, "", "");
				// Before the service's replies
				user_send_take(u, dc, echo);
			}
			if (cstr_eq(cmd, "PRIVMSG")) {
				service_privmsg(u, dc, irc_msg_param(msg, 1));
			}
			goto out;
		}
		if (net == nullptr && cstr_eq(irc_msg_param(msg, 0), dc->nick)) {
			struct irc_msg *echo = irc_msg_clone(msg);
			irc_msg_set_prefix(echo, DOWNSTREAM_SELF_PREFIX, "", "");
			user_packet_add_take(p, echo);
			goto out;
		}
		if (uc == nullptr) {
			fail(p, cmd, "DISCONNECTED", irc_msg_param(msg, 0),
			     "Disconnected from network");
			goto out;
		}
		struct irc_msg *outgoing = irc_msg_clone(msg);
		irc_msg_clear_prefix(outgoing);
		irc_tags_clear(&outgoing->tags);
		copy_client_tags(&outgoing->tags, &msg->tags);
		const char *label = enabled(dc, "labeled-response")
					    ? irc_tags_get_or_empty(&msg->tags, "label")
					    : "";
		bool upstream_echo = cap_registry_is_enabled(&uc->caps, "echo-message");
		if (!upstream_echo) {
			struct irc_msg *local = irc_msg_clone(outgoing);
			irc_msg_set_prefix(local, uc->nick, uc->username, uc->hostname);
			user_produce(u, net, local, irc_msg_param(msg, 0), dc->id, dc->id);
			irc_msg_free(local);
		}
		upstream_send_labeled_take(uc, dc->id, label, outgoing);
		if (upstream_echo) {
			no_ack(p);
		}
	} else if (uc != nullptr) {
		const char *label = enabled(dc, "labeled-response")
					    ? irc_tags_get_or_empty(&msg->tags, "label")
					    : "";
		if (cstr_eq(cmd, "WHO") || cstr_eq(cmd, "WHOIS") || cstr_eq(cmd, "LIST") ||
		    cstr_eq(cmd, "REGISTER") || cstr_eq(cmd, "VERIFY")) {
			bool queued = upstream_enqueue_take(uc, dc->id, label, irc_msg_clone(msg));
			(void)queued;
		} else if (cstr_eq(cmd, "PASS") || cstr_eq(cmd, "USER")) {
			user_packet_add_take(p, IRC_MSG("462", dc->nick, "You may not reregister"));
			goto out;
		} else {
			upstream_send_labeled_take(uc, dc->id, label, irc_msg_clone(msg));
		}
		no_ack(p);
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
	} else if (cstr_eq(cmd, "MODE") && cstr_eq(irc_msg_param(msg, 0), dc->nick)) {
		user_packet_add_take(p, IRC_MSG("221", dc->nick, "+"));
	} else if (cstr_eq(cmd, "JOIN") || cstr_eq(cmd, "PART") || cstr_eq(cmd, "TOPIC") ||
		   cstr_eq(cmd, "NAMES") || cstr_eq(cmd, "KICK") || cstr_eq(cmd, "INVITE")) {
		user_packet_add_take(
			p, IRC_MSG("400", "*", cmd,
				   "Cannot interact with channels and users on the bouncer "
				   "connection. Did you mean to use a specific network?"));
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
