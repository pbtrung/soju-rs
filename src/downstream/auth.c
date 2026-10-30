// SASL and PASS authentication jobs, completed by looking the downstream
// up by ID (soju-rs downstream/registration.rs)
#include "downstream/internal.h"

#include <openssl/evp.h>
#include <stdio.h>
#include <string.h>

#include "auth/auth.h"
#include "base/alloc.h"
#include "base/base64.h"
#include "base/utf8.h"
#include "db/db.h"
#include "irc/genmsg.h"
#include "irc/ircutil.h"
#include "irc/numerics.h"
#include "net/url.h"
#include "user/user.h"

// Asynchronous SASL: the completion looks the downstream up by ID
struct sasl_job {
	uint64_t dc_id;
	struct server *srv;
	char *mechanism;
	char *nick;
	// PLAIN
	char *username, *client, *network, *identity_username, *identity_client, *identity_network;
	bool impersonating;
	// EXTERNAL
	struct strbuf fingerprint;
	struct db_device_certificate cert;
	int64_t user_id;
	char *ip;
	// results
	char *err;
	bool found;
	struct db_user user;
};

static void sasl_job_free(struct sasl_job *j) {
	xfree(j->mechanism);
	xfree(j->nick);
	xfree(j->username);
	xfree(j->client);
	xfree(j->network);
	xfree(j->identity_username);
	xfree(j->identity_client);
	xfree(j->identity_network);
	strbuf_free(&j->fingerprint);
	db_device_certificate_free(&j->cert);
	xfree(j->ip);
	xfree(j->err);
	if (j->found) {
		db_user_free(&j->user);
	}
	xfree(j);
}

// Failed SASL attempts after which an unregistered connection is closed:
// each one costs a password hash, and the hashing queue is shared by
// everyone's logins
static constexpr unsigned MAX_AUTH_FAILURES = 3;

// Ends an asynchronous SASL exchange: err nullptr on success
static void sasl_finish(struct sasl_job *j, const char *internal, const char *external) {
	struct downstream *dc = downstream_lookup(j->dc_id);
	if (dc == nullptr) {
		sasl_job_free(j);
		return;
	}
	if (internal != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(dc->reg->nick));
		log_info(&dc->log, "SASL %s authentication error for nick %s: %s", j->mechanism, q,
			 internal);
		downstream_end_sasl(
			dc, IRC_MSG(ERR_SASLFAIL, dc->nick,
				    external != nullptr ? external : "Authentication failed"));
		if (dc->reg != nullptr && ++dc->reg->auth_failures >= MAX_AUTH_FAILURES) {
			sasl_job_free(j);
			dc->busy = false;
			downstream_reply_and_close(
				dc, IRC_MSG("ERROR", "Too many authentication failures"));
			return;
		}
	} else {
		bool impersonating = j->impersonating;
		downstream_set_auth_username(dc, impersonating ? j->identity_username : j->username,
					     impersonating ? j->identity_client : j->client,
					     impersonating ? j->identity_network : j->network);
		dc->impersonating = impersonating;
		// Technically we should send RPL_LOGGEDIN here. However we use
		// RPL_LOGGEDIN to mirror the upstream connection status. See:
		// https://github.com/ircv3/ircv3-specifications/pull/476
		downstream_end_sasl(dc, nullptr);
	}
	sasl_job_free(j);
	dc->busy = false;
	downstream_resume(dc);
}

static void run_get_admin(struct db *db, void *data) {
	struct sasl_job *j = data;
	struct err *e = nullptr;
	bool found;
	struct db_user u;
	if (!db_get_user(db, j->username, &u, &found, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (!found) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->username));
		j->err = xasprintf("sql: no rows in result set (username %s)", q);
		return;
	}
	bool admin = u.admin;
	db_user_free(&u);
	if (!admin) {
		j->err = xstrdup("SASL impersonation only allowed for admin users");
		return;
	}
	if (!db_get_user(db, j->identity_username, &u, &found, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (!found) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->identity_username));
		j->err = xasprintf("sql: no rows in result set (username %s)", q);
		return;
	}
	db_user_free(&u);
}

static void done_get_admin(void *data) {
	struct sasl_job *j = data;
	sasl_finish(j, j->err, nullptr);
}

static void plain_auth_done(void *data, const struct auth_error *err) {
	struct sasl_job *j = data;
	if (err != nullptr) {
		// Like upstream (which formats the error with %v), this drops the
		// user-facing message: the client gets "Authentication failed"
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->username));
		AUTOFREE char *internal = xasprintf("%s (username %s)", err->internal, q);
		sasl_finish(j, internal, nullptr);
		return;
	}
	if (j->impersonating) {
		server_db(j->srv, run_get_admin, done_get_admin, j);
		return;
	}
	sasl_finish(j, nullptr, nullptr);
}

// OAUTHBEARER: identity_username holds the username the client gave, if any
static void bearer_auth_done(void *data, const char *username, const struct auth_error *err) {
	struct sasl_job *j = data;
	if (err != nullptr) {
		sasl_finish(j, err->internal, err->external);
		return;
	}
	if (j->identity_username[0] != '\0' && !cstr_eq(j->identity_username, username)) {
		AUTOFREE char *a = utf8_debug_cstr(str_from(j->identity_username));
		AUTOFREE char *b = utf8_debug_cstr(str_from(username));
		AUTOFREE char *msg = xasprintf(
			"username mismatch (client provided %s, but server returned %s)", a, b);
		sasl_finish(j, msg, nullptr);
		return;
	}
	xfree(j->username);
	j->username = xstrdup(username);
	sasl_finish(j, nullptr, nullptr);
}

static void run_external(struct db *db, void *data) {
	struct sasl_job *j = data;
	struct err *e = nullptr;
	bool found;
	if (!db_get_device_certificate(db, strbuf_str(&j->fingerprint), &j->user_id, &j->cert,
				       &found, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (!found) {
		j->err = xstrdup("Unknown TLS client certificate");
		return;
	}
	char *name = nullptr;
	if (!db_get_username_by_id(db, j->user_id, &name, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (name == nullptr) {
		j->err = xstrdup("sql: no rows in result set");
		return;
	}
	j->username = name;
	db_device_certificate_mark_used(&j->cert, j->ip);
	if (!db_store_device_certificate(db, j->user_id, &j->cert, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
	}
}

static void done_external(void *data) {
	struct sasl_job *j = data;
	sasl_finish(j, j->err, nullptr);
}

// Starts checking SASL credentials; dc is busy until sasl_finish
void downstream_authenticate(struct downstream *dc, const struct downstream_sasl *s) {
	struct sasl_job *j = xcalloc(1, sizeof(*j));
	j->dc_id = dc->id;
	j->srv = dc->srv;
	j->mechanism = xstrdup(s->mechanism);
	j->client = xstrdup("");
	j->network = xstrdup("");
	struct server_config *cfg = server_config_get(dc->srv);
	enum plain_auth plain = cfg->plain_auth;
	bool oauth = cfg->oauth2 != nullptr;
	bool client_cert_auth = cfg->cfg.basic.client_cert_auth;
	server_config_unref(cfg);
	dc->busy = true;
	if (s->kind == SASL_PLAIN) {
		xfree(j->client);
		xfree(j->network);
		unmarshal_username(s->plain.username, &j->username, &j->client, &j->network);
		if (plain == PLAIN_AUTH_NONE) {
			sasl_finish(j, "SASL PLAIN not supported", nullptr);
			return;
		}
		if (s->plain.identity[0] != '\0' &&
		    !cstr_eq(s->plain.identity, s->plain.username)) {
			j->impersonating = true;
			unmarshal_username(s->plain.identity, &j->identity_username,
					   &j->identity_client, &j->identity_network);
		}
		auth_plain(dc->srv, j->username, s->plain.password, dc->remote_addr,
			   plain_auth_done, j);
		return;
	}
	if (s->kind == SASL_OAUTHBEARER) {
		if (!oauth) {
			sasl_finish(j, "SASL OAUTHBEARER not supported", nullptr);
			return;
		}
		xfree(j->identity_username);
		j->identity_username = xstrdup(s->oauth.username);
		auth_oauth_bearer(dc->srv, s->oauth.token, dc->remote_addr, bearer_auth_done, j);
		return;
	}
	if (s->kind == SASL_EXTERNAL) {
		if (!client_cert_auth) {
			sasl_finish(j, "SASL EXTERNAL not supported", nullptr);
			return;
		}
		if (!dc->has_peer_cert) {
			sasl_finish(j, "TLS client certificate not sent", nullptr);
			return;
		}
		unsigned char md[EVP_MAX_MD_SIZE];
		unsigned len = 0;
		EVP_Digest(dc->peer_cert.buf, dc->peer_cert.len, md, &len, EVP_sha512(), nullptr);
		strbuf_append(&j->fingerprint, (const char *)md, len);
		db_device_certificate_init(&j->cert);
		if (dc->has_remote_ip) {
			struct strbuf ip = STRBUF_INIT;
			ipaddr_format(&ip, dc->remote_ip);
			j->ip = strbuf_take(&ip);
		}
		server_db(dc->srv, run_external, done_external, j);
		return;
	}
	sasl_finish(j, "unsupported SASL mechanism", nullptr);
}
