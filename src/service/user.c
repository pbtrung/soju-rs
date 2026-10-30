#include "service/internal.h"

struct user_change {
	struct service_context *c;
	int64_t target;
	char *name, *password, *hash, *nick, *realname;
	bool create, other, disable, has_admin, admin, has_enabled, enabled, has_max;
	int64_t max;
	struct db_user record;
	struct err *err;
};
static void change_free(struct user_change *j) {
	xfree(j->name);
	xfree(j->password);
	xfree(j->hash);
	xfree(j->nick);
	xfree(j->realname);
	db_user_free(&j->record);
	err_free(j->err);
	xfree(j);
}
static void change_drop(void *data) {
	struct user_change *j = data;
	service_drop(j->c);
	change_free(j);
}
static void store_user(struct db *db, void *data) {
	struct user_change *j = data;
	bool ok;
	if (j->create) {
		struct db_user existing;
		bool found = false;
		if (!db_get_user(db, j->record.username, &existing, &found, &j->err)) {
			return;
		}
		if (found) {
			db_user_free(&existing);
			AUTOFREE char *q = utf8_debug_cstr(str_from(j->record.username));
			ok = err_set(&j->err, 0, "could not create user: user %s already exists",
				     q);
			(void)ok;
			return;
		}
		ok = db_store_user(db, &j->record, &j->err);
	} else {
		struct db_user current;
		bool found = false;
		if (!db_get_user(db, j->record.username, &current, &found, &j->err)) {
			return;
		}
		if (!found || current.id != j->target) {
			if (found) {
				db_user_free(&current);
			}
			ok = err_set(&j->err, 0, "user no longer exists");
			(void)ok;
			return;
		}
		db_user_free(&j->record);
		j->record = current;
		service_replace(&j->record.password, j->disable ? "" : j->hash);
		service_replace(&j->record.nick, j->nick);
		service_replace(&j->record.realname, j->realname);
		if (j->has_admin) {
			j->record.admin = j->admin;
		}
		if (j->has_enabled) {
			j->record.enabled = j->enabled;
		}
		if (j->has_max) {
			j->record.max_networks = j->max;
		}
		ok = (j->hash != nullptr || j->disable)
			     ? db_store_user(db, &j->record, &j->err)
			     : db_store_user_keep_password(db, &j->record, &j->err);
	}
	(void)ok;
}
static void apply_change(struct user *u, void *data) {
	struct user_change *j = data;
	struct service_context *c = j->c;
	if (j->err != nullptr) {
		c->err = j->err;
		j->err = nullptr;
	} else {
		bool was_enabled = u->record.enabled;
		bool nick_changed = !cstr_eq(u->record.nick, j->record.nick);
		bool realname_changed = !cstr_eq(u->record.realname, j->record.realname);
		db_user_free(&u->record);
		db_user_copy(&u->record, &j->record);
		for (size_t i = 0; i < u->networks.len; i++) {
			struct user_network *net = u->networks.data[i];
			struct upstream *uc = net->upstream;
			if (uc != nullptr) {
				db_user_free(&uc->user);
				db_user_copy(&uc->user, &u->record);
			}
			if (uc != nullptr && nick_changed && net->record.nick[0] == '\0') {
				upstream_send_take(
					uc, IRC_MSG("NICK", db_get_nick(&u->record, &net->record)));
			}
			bool reconnect_realname =
				realname_changed && net->record.realname[0] == '\0';
			if (reconnect_realname && uc != nullptr &&
			    cap_registry_is_enabled(&uc->caps, "setname")) {
				upstream_send_take(
					uc, IRC_MSG("SETNAME",
						    db_get_realname(&u->record, &net->record)));
				reconnect_realname = false;
			}
			if ((was_enabled && !u->record.enabled) || reconnect_realname) {
				upstream_free(uc);
				net->upstream = nullptr;
				net->joined_generation = 0;
				user_notify_network(u, net);
				if (u->record.enabled) {
					user_network_start(u, net);
				}
			} else if (!was_enabled && u->record.enabled) {
				user_network_start(u, net);
			}
		}
		service_printf(c, "updated user \"%s\"", u->record.username);
		if (was_enabled && !u->record.enabled) {
			user_close_clients(u, "User disabled");
		} else if (j->other && (j->hash != nullptr || j->disable)) {
			user_close_clients(u, "Password changed");
		}
	}
	service_finish(c);
	change_free(j);
}
static void update_done(void *data) {
	struct user_change *j = data;
	user_post(j->c->srv, j->target, apply_change, change_drop, j);
}
static void prepare_change(struct user *u, void *data) {
	struct user_change *j = data;
	if (uv_hrtime() > j->c->deadline) {
		bool ok = SFAIL(j->c, "context deadline exceeded");
		(void)ok;
		service_finish(j->c);
		change_free(j);
		return;
	}
	db_user_free(&j->record);
	db_user_copy(&j->record, &u->record);
	server_db(j->c->srv, store_user, update_done, j);
}
static void create_announce(struct user *u, void *data) {
	(void)u;
	struct user_change *j = data;
	service_printf(j->c, "created user \"%s\"", j->record.username);
	service_finish(j->c);
	change_free(j);
}
static void create_done(void *data) {
	struct user_change *j = data;
	struct service_context *c = j->c;
	if (j->err != nullptr) {
		c->err = j->err;
		j->err = nullptr;
		service_finish(c);
		change_free(j);
	} else if (!atomic_load(&c->srv->shutting_down)) {
		struct db_user *record = xmalloc(sizeof(*record));
		db_user_copy(record, &j->record);
		int64_t id = record->id;
		user_start_take(c->srv, record);
		user_post(c->srv, id, create_announce, change_drop, j);
	} else {
		service_finish(c);
		change_free(j);
	}
}
static void hash_work(void *data) {
	struct user_change *j = data;
	struct db_user tmp;
	db_user_init(&tmp, "");
	if (db_user_set_password(&tmp, j->password, &j->err)) {
		j->hash = xstrdup(tmp.password);
	}
	db_user_free(&tmp);
}
static void change_continue(struct user_change *j) {
	struct service_context *c = j->c;
	if (j->err != nullptr) {
		c->err = j->err;
		j->err = nullptr;
		service_finish(c);
		change_free(j);
		return;
	}
	if (atomic_load(&c->srv->shutting_down) || uv_hrtime() > c->deadline) {
		bool ok = SFAIL(c, "context deadline exceeded");
		(void)ok;
		service_finish(c);
		change_free(j);
		return;
	}
	if (j->create) {
		service_replace(&j->record.password, j->hash);
		server_db(c->srv, store_user, create_done, j);
	} else {
		user_post(c->srv, j->target, prepare_change, change_drop, j);
	}
}
static void hash_done(void *data, const char *err) {
	struct user_change *j = data;
	if (err != nullptr) {
		bool ok = err_set(&j->err, 0, "%s", err);
		(void)ok;
	}
	change_continue(j);
}
static struct user_change *change_new(struct service_context *c) {
	struct user_change *j = xcalloc(1, sizeof(*j));
	j->c = c;
	db_user_init(&j->record, "");
	return j;
}
HANDLER(user_create) {
	(void)u;
	struct flags_parsed p = {0};
	bool ok = service_parse(c, &p,
				"username:s password:s disable-password:b nick:s realname:s "
				"admin:b enabled:b max-networks:i",
				args, n) &&
		  service_no_args(c, &p);
	struct user_change *j = change_new(c);
	j->create = true;
	const char *username = service_flag_string(&p, "username", ""),
		   *password = flags_get(&p, "password");
	(void)flags_get_bool(&p, "disable-password", &j->disable);
	if (ok && (username[0] == '\0')) {
		ok = SFAIL(c, "flag -username is required");
	}
	if (ok && password != nullptr && password[0] && j->disable) {
		ok = SFAIL(c, "flags -password and -disable-password are mutually exclusive");
	}
	if (ok && (password == nullptr || password[0] == '\0') && !j->disable) {
		ok = SFAIL(c, "flag -password is required");
	}
	if (ok && strpbrk(username, " /@\r\n") != nullptr) {
		ok = SFAIL(c, "invalid username");
	}
	if (ok) {
		service_replace(&j->record.username, username);
		service_replace(&j->record.nick, flags_get(&p, "nick"));
		service_replace(&j->record.realname, flags_get(&p, "realname"));
		(void)flags_get_bool(&p, "admin", &j->record.admin);
		(void)flags_get_bool(&p, "enabled", &j->record.enabled);
		long long max = -1;
		(void)flags_get_int(&p, "max-networks", &max);
		j->record.max_networks = max;
		service_defer(c);
		if (j->disable) {
			change_continue(j);
		} else {
			j->password = xstrdup(password);
			server_cpu(c->srv, hash_work, hash_done, j);
		}
	} else {
		change_free(j);
	}
	flags_parsed_free(&p);
	return ok ? SERVICE_DEFERRED : false;
}
HANDLER(user_update) {
	const char *name = "";
	if (n && args[0][0] != '-') {
		name = args[0];
		args++;
		n--;
	}
	struct flags_parsed p = {0};
	bool ok = service_parse(c, &p,
				"password:s disable-password:b nick:s realname:s admin:v enabled:v "
				"max-networks:a",
				args, n) &&
		  service_no_args(c, &p);
	struct user_change *j = change_new(c);
	j->other = name[0] && (u == nullptr || !cstr_eq(name, u->record.username));
	if (ok && name[0] == '\0' && u == nullptr) {
		ok = SFAIL(c, "cannot determine the user to update");
	}
	j->name = xstrdup(name);
	const char *pw = flags_get(&p, "password");
	j->password = pw != nullptr ? xstrdup(pw) : nullptr;
	j->nick = flags_get(&p, "nick") != nullptr ? xstrdup(flags_get(&p, "nick")) : nullptr;
	j->realname =
		flags_get(&p, "realname") != nullptr ? xstrdup(flags_get(&p, "realname")) : nullptr;
	(void)flags_get_bool(&p, "disable-password", &j->disable);
	j->has_admin = flags_get_bool(&p, "admin", &j->admin);
	j->has_enabled = flags_get_bool(&p, "enabled", &j->enabled);
	long long max = 0;
	j->has_max = flags_get_int(&p, "max-networks", &max);
	j->max = max;
	if (ok && pw != nullptr && j->disable) {
		ok = SFAIL(c, "flags -password and -disable-password are mutually exclusive");
	}
	if (ok && j->other) {
		if (!c->admin) {
			ok = SFAIL(c, "you must be an admin to update other users");
		} else if (j->nick != nullptr) {
			ok = SFAIL(c, "cannot update -nick of other user");
		} else if (j->realname != nullptr) {
			ok = SFAIL(c, "cannot update -realname of other user");
		} else if (!server_find_user(c->srv, name, &j->target)) {
			ok = SFAIL(c, "unknown username \"%s\"", name);
		}
	} else if (ok && u != nullptr) {
		j->target = u->record.id;
		if (j->has_admin) {
			ok = SFAIL(c, "cannot update -admin of own user");
		} else if (j->has_enabled) {
			ok = SFAIL(c, "cannot update -enabled of own user");
		} else if (j->has_max && !c->admin) {
			ok = SFAIL(c, "cannot update -max-networks of own user");
		}
	}
	if (ok) {
		service_defer(c);
		if (j->password != nullptr) {
			server_cpu(c->srv, hash_work, hash_done, j);
		} else {
			change_continue(j);
		}
	} else {
		change_free(j);
	}
	flags_parsed_free(&p);
	return ok ? SERVICE_DEFERRED : false;
}
struct run_job {
	struct service_context *c;
	strvec words;
	bool reset;
};
static void run_drop(void *data) {
	struct run_job *j = data;
	service_drop(j->c);
	strvec_free(&j->words);
	xfree(j);
}
static void run_user(struct user *u, void *data) {
	struct run_job *j = data;
	struct service_context *c = j->c;
	c->user_id = u->record.id;
	// Only an administrator can dispatch user run. Preserve that authority,
	// as Rust does, even when the target account is not an administrator.
	if (j->reset) {
		c->network_id = 0;
		service_replace(&c->nick, "");
		strbuf_free(&c->peer_cert);
		c->has_remote_ip = false;
	}
	service_run(u, c, (const char *const *)j->words.data, j->words.len);
	strvec_free(&j->words);
	xfree(j);
}
HANDLER(user_run) {
	if (n < 2) {
		return SFAIL(c, "expected at least two arguments");
	}
	int64_t id = 0;
	if (!server_find_user(c->srv, args[0], &id)) {
		return SFAIL(c, "unknown username \"%s\"", args[0]);
	}
	// Even same-user commands are queued, avoiding reentrant mutation/free of
	// the current dispatch context. The original deadline bounds nested run.
	struct run_job *j = xcalloc(1, sizeof(*j));
	j->c = c;
	j->reset = u == nullptr || id != u->record.id;
	for (size_t i = 1; i < n; i++) {
		strvec_push(&j->words, args[i]);
	}
	service_defer(c);
	user_post(c->srv, id, run_user, run_drop, j);
	return SERVICE_DEFERRED;
}
struct delete_job {
	struct service_context *c;
	int64_t id;
	char *name;
	struct err *err;
};
static void delete_db(struct db *db, void *data) {
	struct delete_job *j = data;
	bool ok = db_delete_user(db, j->id, &j->err);
	(void)ok;
}
static void delete_done(void *data) {
	struct delete_job *j = data;
	if (j->err != nullptr) {
		j->c->err = j->err;
		j->err = nullptr;
	} else {
		service_printf(j->c, "deleted user \"%s\"", j->name);
	}
	service_finish(j->c);
	xfree(j->name);
	xfree(j);
}
static void delete_drop(void *data) {
	struct delete_job *j = data;
	service_drop(j->c);
	xfree(j->name);
	xfree(j);
}
static void delete_user(struct user *u, void *data) {
	struct delete_job *j = data;
	if (uv_hrtime() > j->c->deadline) {
		bool ok = SFAIL(j->c, "context deadline exceeded");
		(void)ok;
		service_finish(j->c);
		xfree(j->name);
		xfree(j);
		return;
	}
	user_stop(u);
	server_db(j->c->srv, delete_db, delete_done, j);
}
HANDLER(user_delete) {
	if (n != 1 && n != 2) {
		return SFAIL(c, "expected one or two arguments");
	}
	bool self = u != nullptr && cstr_eq(u->record.username, args[0]);
	if (!c->admin && !self) {
		return SFAIL(c, "only admins may delete other users");
	}
	int64_t id = 0;
	if (!server_find_user(c->srv, args[0], &id)) {
		return SFAIL(c, "unknown username \"%s\"", args[0]);
	}
	AUTOFREE char *hash = service_digest("SHA1", str_from(args[0]));
	if (strlen(hash) < 6) {
		return SFAIL(c, "failed to compute confirmation token");
	}
	hash[6] = '\0';
	if (n == 1) {
		service_printf(c, "To confirm user deletion, send \"user delete %s %s\"", args[0],
			       hash);
		return true;
	}
	if (!cstr_eq(hash, args[1])) {
		return SFAIL(c, "provided confirmation token doesn't match user");
	}
	struct delete_job *j = xcalloc(1, sizeof(*j));
	*j = (struct delete_job){.c = c, .id = id, .name = xstrdup(args[0])};
	if (self) {
		service_printf(
			c,
			"Goodbye %s, deleting your account. There will be no further confirmation.",
			args[0]);
	}
	service_defer(c);
	user_post(c->srv, id, delete_user, delete_drop, j);
	return SERVICE_DEFERRED;
}
struct status_job {
	const char *name;
	db_user_vec users;
	i64vec counts;
	struct err *err;
};
static void status_db(struct db *db, void *data) {
	struct status_job *j = data;
	if (!db_list_users(db, &j->users, &j->err)) {
		return;
	}
	for (size_t i = 0; i < j->users.len; i++) {
		db_network_vec networks = {0};
		bool ok = db_list_networks(db, j->users.data[i].id, &networks, &j->err);
		vec_push(&j->counts, (int64_t)networks.len);
		db_network_vec_free(&networks);
		if (!ok) {
			return;
		}
	}
}
HANDLER(user_status) {
	(void)u;
	if (n > 1) {
		return SFAIL(c, "expected 0 or 1 argument");
	}
	struct status_job j = {0};
	db_thread_call(c->srv->db, status_db, &j);
	bool ok = j.err == nullptr;
	if (!ok) {
		c->err = j.err;
	}
	size_t printed = 0;
	for (size_t i = 0; ok && i < j.users.len; i++) {
		struct db_user *r = &j.users.data[i];
		if (n && !cstr_eq(r->username, args[0])) {
			continue;
		}
		if (!n && printed >= 50) {
			break;
		}
		AUTO_STRBUF struct strbuf line = {0};
		strbuf_append_cstr(&line, r->username);
		if (r->admin || !r->enabled) {
			strbuf_printf(&line, " (%s%s%s)", r->admin ? "admin" : "",
				      r->admin && !r->enabled ? ", " : "",
				      !r->enabled ? "disabled" : "");
		}
		strbuf_printf(&line, ": %lld networks", (long long)j.counts.data[i]);
		if (r->max_networks >= 0) {
			strbuf_printf(&line, " (%lld max)", (long long)r->max_networks);
		}
		service_print(c, strbuf_cstr(&line));
		printed++;
	}
	if (ok && n && !printed) {
		ok = SFAIL(c, "unknown username \"%s\"", args[0]);
	}
	if (ok && !n && j.users.len > 50) {
		service_printf(c, "(%zu more users omitted)", j.users.len - 50);
	}
	db_user_vec_free(&j.users);
	vec_free(&j.counts);
	return ok;
}
