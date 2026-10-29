#include "service/internal.h"
struct stats_job {
	struct db_stats stats;
	struct err *err;
};
static void stats_db(struct db *db, void *data) {
	struct stats_job *j = data;
	bool ok = db_stats(db, &j->stats, &j->err);
	(void)ok;
}
HANDLER(server_status) {
	(void)u;
	(void)args;
	if (n) {
		return SFAIL(c, "expected no argument");
	}
	struct stats_job j = {0};
	db_thread_call(c->srv->db, stats_db, &j);
	if (j.err != nullptr) {
		c->err = j.err;
		return false;
	}
	pthread_mutex_lock(&c->srv->users_mu);
	size_t users = strmap_len(&c->srv->users);
	pthread_mutex_unlock(&c->srv->users_mu);
	service_printf(
		c, "%zu/%lld users, %lld downstreams, %lld upstreams, %lld networks, %lld channels",
		users, (long long)j.stats.users,
		(long long)atomic_load(&c->srv->metrics.downstreams),
		(long long)atomic_load(&c->srv->metrics.upstreams), (long long)j.stats.networks,
		(long long)j.stats.channels);
	return true;
}
HANDLER(server_notice) {
	(void)u;
	if (n != 1) {
		return SFAIL(c, "expected exactly one argument");
	}
	AUTOFREE char *host = server_hostname(c->srv);
	AUTOFREE char *target = xasprintf("$%s", host);
	struct irc_msg *msg = IRC_MSG("NOTICE", target, args[0]);
	irc_msg_set_prefix(msg, SERVICE_NICK, SERVICE_NICK, host);
	pthread_mutex_lock(&c->srv->users_mu);
	size_t count = strmap_len(&c->srv->users);
	pthread_mutex_unlock(&c->srv->users_mu);
	log_info(&c->srv->log, "broadcasting bouncer-wide NOTICE: %s", args[0]);
	user_broadcast_take(c->srv, msg);
	service_printf(c, "sent to %zu/%zu downstream connections", count, count);
	return true;
}
HANDLER(server_debug) {
	(void)u;
	if (n != 1) {
		return SFAIL(c, "expected exactly one argument");
	}
	bool enabled = false;
	if (!parse_go_bool(str_from(args[0]), &enabled)) {
		return SFAIL(c, "strconv.ParseBool: parsing \"%s\": invalid syntax", args[0]);
	}
	bool prev = log_set_debug(&c->srv->log, enabled);
	if (prev != enabled) {
		log_info(&c->srv->log, "%s debug logging", enabled ? "enabling" : "disabling");
	}
	return true;
}
