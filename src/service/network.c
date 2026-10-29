#include "service/internal.h"

static constexpr char network_flags[] =
	"addr:s name:s nick:s username:s pass:s realname:s certfp:s auto-away:v enabled:v "
	"ignore-limit:b connect-command:s";
static bool update(struct service_context *c, const struct flags_parsed *p, struct db_network *r) {
	if (cstr_eq(service_flag_string(p, "name", ""), "*")) {
		return SFAIL(c, "the network name \"*\" is reserved");
	}
	bool ignore = false;
	(void)flags_get_bool(p, "ignore-limit", &ignore);
	if (ignore && !c->admin) {
		return SFAIL(c, "you must be an admin to use the flag -ignore-limit");
	}
	service_replace(&r->addr, flags_get(p, "addr"));
	service_replace(&r->name, flags_get(p, "name"));
	service_replace(&r->nick, flags_get(p, "nick"));
	service_replace(&r->username, flags_get(p, "username"));
	service_replace(&r->pass, flags_get(p, "pass"));
	service_replace(&r->realname, flags_get(p, "realname"));
	const char *fp = flags_get(p, "certfp");
	if (fp != nullptr) {
		AUTO_STRBUF struct strbuf bytes = {0};
		if (!service_unhex(fp, &bytes)) {
			return SFAIL(c, "the certificate fingerprint must be hex-encoded");
		}
		if (bytes.len != 0 && bytes.len != 32 && bytes.len != 64) {
			return SFAIL(c,
				     "the certificate fingerprint must be a SHA256 or SHA512 hash");
		}
		AUTOFREE char *hex = service_hex(strbuf_str(&bytes), false);
		AUTOFREE char *value =
			bytes.len ? xasprintf("sha-%u:%s", bytes.len == 32 ? 256 : 512, hex)
				  : xstrdup("");
		service_replace(&r->certfp, value);
	}
	(void)flags_get_bool(p, "auto-away", &r->auto_away);
	(void)flags_get_bool(p, "enabled", &r->enabled);
	const strvec *commands = flags_get_all(p, "connect-command");
	if (commands != nullptr) {
		if (commands->len > 20) {
			return SFAIL(c, "too many -connect-command flags supplied");
		}
		bool clear = commands->len == 1 && commands->data[0][0] == '\0';
		if (!clear) {
			for (size_t i = 0; i < commands->len; i++) {
				struct irc_msg *m =
					irc_msg_parse(str_from(commands->data[i]), nullptr);
				if (m == nullptr) {
					return SFAIL(c, "flag -connect-command must be a valid raw "
							"irc command string");
				}
				irc_msg_free(m);
			}
		}
		strvec_free(&r->connect_commands);
		if (!clear) {
			strvec_copy(&r->connect_commands, commands);
		}
	}
	return true;
}
HANDLER(network_create) {
	struct flags_parsed p = {0};
	bool ok = service_parse(c, &p, network_flags, args, n) && service_no_args(c, &p);
	struct db_network r;
	db_network_init(&r, service_flag_string(&p, "addr", ""));
	if (ok && flags_get(&p, "addr") == nullptr) {
		ok = SFAIL(c, "flag -addr is required");
	}
	bool ignore = false;
	(void)flags_get_bool(&p, "ignore-limit", &ignore);
	if (ok) {
		ok = update(c, &p, &r) && user_store_network(u, &r, !ignore, &c->err);
	}
	if (ok) {
		service_printf(c, "created network \"%s\"", db_network_get_name(&r));
	}
	db_network_free(&r);
	flags_parsed_free(&p);
	return ok;
}
HANDLER(network_update) {
	const char *name = "";
	if (n && args[0][0] != '-') {
		name = args[0];
		args++;
		n--;
	}
	struct user_network *net = service_network(u, c, name);
	if (net == nullptr) {
		return false;
	}
	struct flags_parsed p = {0};
	bool ok = service_parse(c, &p, network_flags, args, n) && service_no_args(c, &p);
	struct db_network r;
	db_network_copy(&r, &net->record);
	bool ignore = false;
	(void)flags_get_bool(&p, "ignore-limit", &ignore);
	if (ok) {
		ok = update(c, &p, &r) &&
		     user_store_network(u, &r, !ignore && !net->record.enabled, &c->err);
	}
	if (ok) {
		service_printf(c, "updated network \"%s\"", db_network_get_name(&r));
	}
	db_network_free(&r);
	flags_parsed_free(&p);
	return ok;
}
HANDLER(network_status) {
	(void)args;
	if (n) {
		return SFAIL(c, "expected no argument");
	}
	for (size_t i = 0; i < u->networks.len; i++) {
		struct user_network *net = u->networks.data[i];
		struct upstream *uc = net->upstream;
		bool connected = uc != nullptr && uc->attached;
		AUTO_STRBUF struct strbuf status = {0};
		strbuf_append_cstr(&status, connected             ? "connected"
					    : net->record.enabled ? "disconnected"
								  : "disabled");
		if (connected && c->nick[0] && !cstr_eq(c->nick, uc->nick)) {
			strbuf_printf(&status, " as %s", uc->nick);
		}
		if (net->record.id == c->network_id) {
			strbuf_append_cstr(&status, ", current");
		}
		AUTO_STRBUF struct strbuf line = {0};
		strbuf_append_cstr(&line, db_network_get_name(&net->record));
		if (!cstr_eq(db_network_get_name(&net->record), net->record.addr)) {
			strbuf_printf(&line, " (%s)", net->record.addr);
		}
		strbuf_printf(&line, " [%s]", strbuf_cstr(&status));
		if (connected) {
			strbuf_printf(&line, ": %zu channels", cmmap_len(&uc->channels));
		} else if (net->record.enabled && net->last_error[0]) {
			strbuf_printf(&line, ": %s", net->last_error);
		}
		service_print(c, strbuf_cstr(&line));
	}
	if (u->networks.len == 0) {
		service_print(c, "No network configured, add one with \"network create\".");
	}
	return true;
}
HANDLER(network_info) {
	if (n > 1) {
		return SFAIL(c, "unexpected argument: %s", args[1]);
	}
	struct user_network *net = service_network(u, c, n ? args[0] : "");
	if (net == nullptr) {
		return false;
	}
	struct db_network *r = &net->record;
	service_printf(c, "name: %s", db_network_get_name(r));
	service_printf(c, "addr: %s", r->addr);
	service_printf(c, "nick: %s", db_get_nick(&u->record, r));
	service_printf(c, "username: %s", db_get_username(&u->record, r));
	service_printf(c, "realname: %s", db_get_realname(&u->record, r));
	if (r->pass[0]) {
		service_print(c, "pass: (set)");
	}
	if (r->certfp[0]) {
		service_printf(c, "certfp: %s", r->certfp);
	}
	if (cstr_eq(r->sasl.mechanism, "PLAIN")) {
		service_printf(c, "sasl: PLAIN (username: %s)", r->sasl.plain_username);
	} else {
		service_printf(c, "sasl: %s", r->sasl.mechanism[0] ? r->sasl.mechanism : "none");
	}
	service_printf(c, "auto-away: %s", r->auto_away ? "true" : "false");
	service_printf(c, "enabled: %s", r->enabled ? "true" : "false");
	for (size_t i = 0; i < r->connect_commands.len; i++) {
		AUTOFREE char *redacted = service_redact_command(r->connect_commands.data[i]);
		service_printf(c, "connect-command: %s", redacted);
	}
	return true;
}
HANDLER(network_delete) {
	if (n != 1) {
		return SFAIL(c, "expected exactly one argument");
	}
	struct user_network *net = service_network(u, c, args[0]);
	if (net == nullptr) {
		return false;
	}
	AUTOFREE char *name = xstrdup(db_network_get_name(&net->record));
	if (!user_delete_network(u, net->record.id, &c->err)) {
		return false;
	}
	service_printf(c, "deleted network \"%s\"", name);
	return true;
}
HANDLER(network_quote) {
	if (n != 1 && n != 2) {
		return SFAIL(c, "expected one or two arguments");
	}
	struct user_network *net = service_network(u, c, n == 2 ? args[0] : "");
	if (net == nullptr) {
		return false;
	}
	if (net->upstream == nullptr || !net->upstream->attached) {
		return SFAIL(c, "network \"%s\" is not currently connected",
			     db_network_get_name(&net->record));
	}
	struct irc_msg *m = irc_msg_parse(str_from(args[n - 1]), nullptr);
	if (m == nullptr) {
		return SFAIL(c, "failed to parse command");
	}
	upstream_send_take(net->upstream, m);
	service_printf(c, "sent command to \"%s\"", db_network_get_name(&net->record));
	return true;
}
