#include "service/internal.h"
#include <stdlib.h>
static constexpr char channel_flags[] =
	"detached:v relay-detached:s reattach-on:s detach-after:s detach-on:s";
static const char *filter_name(enum message_filter f) {
	static const char *const names[] = {"default", "none", "highlight", "message"};
	return names[f >= FILTER_DEFAULT && f <= FILTER_MESSAGE ? f : FILTER_DEFAULT];
}
static bool filter(struct service_context *c, const char *s, enum message_filter *out) {
	if (s == nullptr) {
		return true;
	}
	for (unsigned i = 0; i < 4; i++) {
		if (cstr_eq(s, filter_name((enum message_filter)i))) {
			*out = (enum message_filter)i;
			return true;
		}
	}
	return SFAIL(c, "unknown filter: \"%s\"", s);
}
static bool update(struct service_context *c, const struct flags_parsed *p, struct db_channel *ch) {
	if (!filter(c, flags_get(p, "relay-detached"), &ch->relay_detached) ||
	    !filter(c, flags_get(p, "reattach-on"), &ch->reattach_on) ||
	    !filter(c, flags_get(p, "detach-on"), &ch->detach_on)) {
		return false;
	}
	const char *s = flags_get(p, "detach-after");
	if (s != nullptr && !service_parse_duration(s, &ch->detach_after_ns)) {
		return SFAIL(c,
			     "unknown duration for -detach-after \"%s\" (duration format: 0, 300s, "
			     "22h30m, ...)",
			     s);
	}
	return true;
}
static struct user_network *channel_network(struct user *u, struct service_context *c,
					    const char *arg, char **name) {
	*name = xstrdup(arg);
	if (c->network_id) {
		return service_network(u, c, "");
	}
	char *slash = strchr(*name, '/');
	if (slash == nullptr) {
		bool ok = SFAIL(c, "missing network name");
		(void)ok;
		return nullptr;
	}
	*slash = '\0';
	return service_network(u, c, slash + 1);
}
static int compare_channels(const void *a, const void *b) {
	const struct db_channel *const *x = a, *const *y = b;
	const char *xs = (*x)->name, *ys = (*y)->name;
	while (*xs == '#') {
		xs++;
	}
	while (*ys == '#') {
		ys++;
	}
	return strcmp(xs, ys);
}
HANDLER(channel_status) {
	struct flags_parsed p = {0};
	bool ok = service_parse(c, &p, "network:s", args, n) && service_no_args(c, &p);
	size_t count = 0;
	const char *name = flags_get(&p, "network");
	struct user_network *selected = nullptr;
	if (ok && (name != nullptr || c->network_id)) {
		selected = service_network(u, c, name != nullptr ? name : "");
		ok = selected != nullptr;
	}
	if (ok) {
		for (size_t i = 0; i < u->networks.len; i++) {
			struct user_network *net = u->networks.data[i];
			if (selected != nullptr && net != selected) {
				continue;
			}
			VEC(struct db_channel *) channels = {0};
			void *v;
			for (size_t it = 0; cmmap_next(&net->channels, &it, nullptr, &v);) {
				vec_push(&channels, v);
			}
			if (channels.len > 1) {
				qsort(channels.data, channels.len, sizeof(*channels.data),
				      compare_channels);
			}
			for (size_t j = 0; j < channels.len; j++) {
				struct db_channel *ch = channels.data[j];
				struct upstream *uc = net->upstream;
				const char *status =
					uc != nullptr && uc->attached
						? (cmmap_has(&uc->channels, ch->name) ? "joined"
										      : "parted")
						: "disconnected";
				AUTOFREE char *duration =
					service_format_duration(ch->detach_after_ns);
				AUTOFREE char *display =
					selected == nullptr
						? xasprintf("%s/%s", ch->name,
							    db_network_get_name(&net->record))
						: xstrdup(ch->name);
				service_printf(c,
					       "%s [%s%s]: relay-detached: %s, reattach-on: %s, "
					       "detach-after: %s, detach-on: %s",
					       display, status, ch->detached ? ", detached" : "",
					       filter_name(ch->relay_detached),
					       filter_name(ch->reattach_on), duration,
					       filter_name(ch->detach_on));
				count++;
			}
			vec_free(&channels);
		}
	}
	if (ok && count == 0) {
		service_print(c, "No channel configured.");
	}
	flags_parsed_free(&p);
	return ok;
}
static bool channel_store(struct user *u, struct service_context *c, const char *const *args,
			  size_t n, bool create) {
	if (n == 0) {
		return SFAIL(c, "expected at least one argument");
	}
	struct flags_parsed p = {0};
	bool ok = service_parse(c, &p, channel_flags, args + 1, n - 1) && service_no_args(c, &p);
	AUTOFREE char *name = nullptr;
	struct user_network *net = ok ? channel_network(u, c, args[0], &name) : nullptr;
	ok = ok && net != nullptr;
	struct db_channel *old = ok ? cmmap_get(&net->channels, name) : nullptr;
	if (ok && (create ? old != nullptr : old == nullptr)) {
		ok = SFAIL(c, create ? "channel \"%s\" already exists" : "unknown channel \"%s\"",
			   name);
	}
	if (ok && create && (name[0] == '\0' || strpbrk(name, " ,\a\r\n:") != nullptr)) {
		ok = SFAIL(c, "invalid channel name: %s", name);
	}
	if (ok && create && net->upstream != nullptr &&
	    strchr(net->upstream->channel_types, name[0]) == nullptr) {
		ok = SFAIL(c, "not a channel name: %s", name);
	}
	struct db_channel ch;
	db_channel_init(&ch);
	if (ok) {
		service_replace(&ch.name, name);
		if (old != nullptr) {
			ch.id = old->id;
			service_replace(&ch.key, old->key);
			service_replace(&ch.detached_internal_msg_id,
					old->detached_internal_msg_id);
			ch.detached = old->detached;
			ch.relay_detached = old->relay_detached;
			ch.reattach_on = old->reattach_on;
			ch.detach_after_ns = old->detach_after_ns;
			ch.detach_on = old->detach_on;
		}
		ok = update(c, &p, &ch);
	}
	bool detached = ch.detached;
	bool has_detached = flags_get_bool(&p, "detached", &detached);
	if (create) {
		ch.detached = detached;
	}
	if (ok) {
		ok = user_store_channel(u, net, &ch, &c->err);
	}
	if (ok) {
		if (!create && has_detached && detached != ch.detached) {
			if (detached) {
				user_detach_channel(u, net, name);
			} else {
				user_attach_channel(u, net, name);
			}
		}
		if (net->upstream != nullptr) {
			if (create) {
				upstream_send_take(net->upstream, IRC_MSG("JOIN", name));
			}
			upstream_set_auto_detach(net->upstream, name, ch.detach_after_ns / 1000000);
		}
		service_printf(c, create ? "created channel \"%s\"" : "updated channel \"%s\"",
			       name);
	}
	db_channel_free(&ch);
	flags_parsed_free(&p);
	return ok;
}
HANDLER(channel_create) {
	return channel_store(u, c, args, n, true);
}
HANDLER(channel_update) {
	return channel_store(u, c, args, n, false);
}
struct delete_channel {
	int64_t id;
	struct err *err;
};
static void delete_channel(struct db *db, void *data) {
	struct delete_channel *j = data;
	bool ok = db_delete_channel(db, j->id, &j->err);
	(void)ok;
}
HANDLER(channel_delete) {
	if (n != 1) {
		return SFAIL(c, "expected exactly one argument");
	}
	AUTOFREE char *name = nullptr;
	struct user_network *net = channel_network(u, c, args[0], &name);
	if (net == nullptr) {
		return false;
	}
	struct db_channel *ch = cmmap_get(&net->channels, name);
	if (ch == nullptr) {
		return SFAIL(c, "unknown channel \"%s\"", name);
	}
	struct delete_channel j = {.id = ch->id};
	db_thread_call(u->srv->db, delete_channel, &j);
	if (j.err != nullptr) {
		c->err = j.err;
		return false;
	}
	cmmap_del(&net->channels, name);
	if (net->upstream != nullptr) {
		upstream_set_auto_detach(net->upstream, name, 0);
		if (cmmap_has(&net->upstream->channels, name)) {
			upstream_send_take(net->upstream, IRC_MSG("PART", name));
		}
	}
	service_printf(c, "deleted channel \"%s\"", name);
	return true;
}
