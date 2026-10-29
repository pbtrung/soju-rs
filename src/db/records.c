// Constructors, copies and destructors of the database records
#include <crypt.h>
#include <string.h>

#include "base/alloc.h"
#include "db/internal.h"

static char *dup_or_empty(const char *s) {
	return xstrdup(s != nullptr ? s : "");
}

static void blob_copy(struct db_blob *dst, const struct db_blob *src) {
	*dst = (struct db_blob){.set = src->set};
	if (src->set) {
		strbuf_append(&dst->data, src->data.buf != nullptr ? src->data.buf : "",
			      src->data.len);
		strbuf_append(&dst->data, "", 0);
	}
}

void db_user_init(struct db_user *u, const char *username) {
	*u = (struct db_user){
		.username = xstrdup(username),
		.password = xstrdup(""),
		.nick = xstrdup(""),
		.realname = xstrdup(""),
		.enabled = true,
		.max_networks = -1,
	};
}

void db_user_copy(struct db_user *dst, const struct db_user *src) {
	*dst = *src;
	dst->username = xstrdup(src->username);
	dst->password = xstrdup(src->password);
	dst->nick = xstrdup(src->nick);
	dst->realname = xstrdup(src->realname);
}

void db_user_free(struct db_user *u) {
	xfree(u->username);
	if (u->password != nullptr) {
		explicit_bzero(u->password, strlen(u->password));
	}
	xfree(u->password);
	xfree(u->nick);
	xfree(u->realname);
	*u = (struct db_user){0};
}

void db_network_init(struct db_network *n, const char *addr) {
	*n = (struct db_network){
		.name = xstrdup(""),
		.addr = xstrdup(addr),
		.nick = xstrdup(""),
		.username = xstrdup(""),
		.realname = xstrdup(""),
		.pass = xstrdup(""),
		.certfp = xstrdup(""),
		.sasl =
			{
				.mechanism = xstrdup(""),
				.plain_username = xstrdup(""),
				.plain_password = xstrdup(""),
			},
		.auto_away = true,
		.enabled = true,
	};
}

void db_network_copy(struct db_network *dst, const struct db_network *src) {
	*dst = *src;
	dst->name = xstrdup(src->name);
	dst->addr = xstrdup(src->addr);
	dst->nick = xstrdup(src->nick);
	dst->username = xstrdup(src->username);
	dst->realname = xstrdup(src->realname);
	dst->pass = xstrdup(src->pass);
	dst->connect_commands = (strvec){0};
	strvec_copy(&dst->connect_commands, &src->connect_commands);
	dst->certfp = xstrdup(src->certfp);
	dst->sasl.mechanism = xstrdup(src->sasl.mechanism);
	dst->sasl.plain_username = xstrdup(src->sasl.plain_username);
	dst->sasl.plain_password = xstrdup(src->sasl.plain_password);
	blob_copy(&dst->sasl.external_cert, &src->sasl.external_cert);
	blob_copy(&dst->sasl.external_key, &src->sasl.external_key);
}

static void free_secret(char *s) {
	if (s != nullptr) {
		explicit_bzero(s, strlen(s));
	}
	xfree(s);
}

static void blob_free_secret(struct db_blob *b) {
	if (b->data.buf != nullptr) {
		explicit_bzero(b->data.buf, b->data.cap);
	}
	strbuf_free(&b->data);
	b->set = false;
}

void db_network_free(struct db_network *n) {
	xfree(n->name);
	xfree(n->addr);
	xfree(n->nick);
	xfree(n->username);
	xfree(n->realname);
	free_secret(n->pass);
	strvec_free(&n->connect_commands);
	xfree(n->certfp);
	xfree(n->sasl.mechanism);
	xfree(n->sasl.plain_username);
	free_secret(n->sasl.plain_password);
	strbuf_free(&n->sasl.external_cert.data);
	blob_free_secret(&n->sasl.external_key);
	*n = (struct db_network){0};
}

const char *db_network_get_name(const struct db_network *n) {
	return n->name[0] != '\0' ? n->name : n->addr;
}

const char *db_get_nick(const struct db_user *u, const struct db_network *n) {
	if (n != nullptr && n->nick[0] != '\0') {
		return n->nick;
	}
	if (u->nick[0] != '\0') {
		return u->nick;
	}
	return u->username;
}

const char *db_get_username(const struct db_user *u, const struct db_network *n) {
	if (n != nullptr && n->username[0] != '\0') {
		return n->username;
	}
	return db_get_nick(u, n);
}

const char *db_get_realname(const struct db_user *u, const struct db_network *n) {
	if (n != nullptr && n->realname[0] != '\0') {
		return n->realname;
	}
	if (u->realname[0] != '\0') {
		return u->realname;
	}
	return db_get_nick(u, n);
}

enum message_filter message_filter_from_i64(int64_t v) {
	switch (v) {
	case 1:
		return FILTER_NONE;
	case 2:
		return FILTER_HIGHLIGHT;
	case 3:
		return FILTER_MESSAGE;
	default:
		return FILTER_DEFAULT;
	}
}

void db_channel_init(struct db_channel *c) {
	*c = (struct db_channel){
		.name = xstrdup(""),
		.key = xstrdup(""),
		.detached_internal_msg_id = xstrdup(""),
	};
}

void db_channel_free(struct db_channel *c) {
	xfree(c->name);
	free_secret(c->key);
	xfree(c->detached_internal_msg_id);
	*c = (struct db_channel){0};
}

void db_device_certificate_init(struct db_device_certificate *c) {
	*c = (struct db_device_certificate){.label = xstrdup(""), .last_ip = xstrdup("")};
}

void db_device_certificate_free(struct db_device_certificate *c) {
	xfree(c->label);
	strbuf_free(&c->fingerprint);
	xfree(c->last_ip);
	*c = (struct db_device_certificate){0};
}

void db_device_certificate_mark_used(struct db_device_certificate *c, const char *ip) {
	c->last_used = (struct db_time){.set = true, .t = timestamp_now()};
	char *v = dup_or_empty(ip);
	xfree(c->last_ip);
	c->last_ip = v;
}

void db_delivery_receipt_init(struct db_delivery_receipt *r) {
	*r = (struct db_delivery_receipt){
		.target = xstrdup(""),
		.client = xstrdup(""),
		.internal_msg_id = xstrdup(""),
	};
}

void db_delivery_receipt_free(struct db_delivery_receipt *r) {
	xfree(r->target);
	xfree(r->client);
	xfree(r->internal_msg_id);
	*r = (struct db_delivery_receipt){0};
}

void db_read_receipt_init(struct db_read_receipt *r) {
	*r = (struct db_read_receipt){.target = xstrdup("")};
}

void db_read_receipt_free(struct db_read_receipt *r) {
	xfree(r->target);
	*r = (struct db_read_receipt){0};
}

void db_webpush_config_init(struct db_webpush_config *c) {
	*c = (struct db_webpush_config){.vapid_public = xstrdup(""), .vapid_private = xstrdup("")};
}

void db_webpush_config_free(struct db_webpush_config *c) {
	xfree(c->vapid_public);
	free_secret(c->vapid_private);
	*c = (struct db_webpush_config){0};
}

void db_webpush_subscription_init(struct db_webpush_subscription *s) {
	*s = (struct db_webpush_subscription){
		.endpoint = xstrdup(""),
		.key_auth = xstrdup(""),
		.key_p256dh = xstrdup(""),
		.key_vapid = xstrdup(""),
	};
}

void db_webpush_subscription_free(struct db_webpush_subscription *s) {
	xfree(s->endpoint);
	free_secret(s->key_auth);
	xfree(s->key_p256dh);
	xfree(s->key_vapid);
	*s = (struct db_webpush_subscription){0};
}

void db_message_target_free(struct db_message_target *t) {
	xfree(t->target);
	*t = (struct db_message_target){0};
}

static void target_last_free(struct db_target_last *t) {
	xfree(t->name);
}

static void message_row_free(struct db_message_row *r) {
	xfree(r->time);
	irc_msg_free(r->message);
}

void db_user_vec_free(db_user_vec *v) {
	vec_free_with(v, db_user_free);
}

void db_network_vec_free(db_network_vec *v) {
	vec_free_with(v, db_network_free);
}

void db_channel_vec_free(db_channel_vec *v) {
	vec_free_with(v, db_channel_free);
}

void db_cert_vec_free(db_cert_vec *v) {
	vec_free_with(v, db_device_certificate_free);
}

void db_receipt_vec_free(db_receipt_vec *v) {
	vec_free_with(v, db_delivery_receipt_free);
}

void db_webpush_config_vec_free(db_webpush_config_vec *v) {
	vec_free_with(v, db_webpush_config_free);
}

void db_webpush_sub_vec_free(db_webpush_sub_vec *v) {
	vec_free_with(v, db_webpush_subscription_free);
}

void db_message_target_vec_free(db_message_target_vec *v) {
	vec_free_with(v, db_message_target_free);
}

void db_target_last_vec_free(db_target_last_vec *v) {
	vec_free_with(v, target_last_free);
}

void db_message_row_vec_free(db_message_row_vec *v) {
	vec_free_with(v, message_row_free);
}

// Passwords

static bool b64_bcrypt_char(char c) {
	return c == '.' || c == '/' || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
	       (c >= '0' && c <= '9');
}

// The checks of the bcrypt crate's split_hash, with its messages; *cost is
// set on success
static bool split_hash(const char *hash, unsigned *cost, struct err **err) {
	size_t len = strlen(hash);
	bool ascii = true;
	for (size_t i = 0; i < len; i++) {
		ascii = ascii && (unsigned char)hash[i] < 0x80;
	}
	if (len != 60 || !ascii) {
		return err_set(
			err, 0,
			"Invalid hash: the hash format is malformed; expected 60 ASCII bytes");
	}
	if (hash[0] != '$' || hash[3] != '$' || hash[6] != '$') {
		return err_set(err, 0, "Invalid hash: the hash format is malformed");
	}
	if (hash[1] != '2' ||
	    (hash[2] != 'y' && hash[2] != 'b' && hash[2] != 'a' && hash[2] != 'x')) {
		return err_set(err, 0, "Invalid hash: the hash prefix is not a bcrypt prefix");
	}
	unsigned long long c = 0;
	if (!str_parse_u64(str_new(hash + 4, 2), &c)) {
		return err_set(err, 0, "Invalid hash: the cost value is not a number");
	}
	for (size_t i = 7; i < 29; i++) {
		if (!b64_bcrypt_char(hash[i])) {
			return err_set(err, 0, "Invalid hash: the salt part is not valid base64");
		}
	}
	for (size_t i = 29; i < 60; i++) {
		if (!b64_bcrypt_char(hash[i])) {
			return err_set(err, 0, "Invalid hash: the hash part is not valid base64");
		}
	}
	if (c < 4 || c > 31) {
		return err_set(err, 0, "Cost needs to be between 4 and 31, got %llu", c);
	}
	*cost = (unsigned)c;
	return true;
}

// bcrypt::verify
static bool bcrypt_verify(const char *password, const char *hash, bool *ok, struct err **err) {
	unsigned cost = 0;
	if (!split_hash(hash, &cost, err)) {
		return false;
	}
	struct crypt_data *data = xcalloc(1, sizeof(*data));
	const char *out = crypt_rn(password, hash, data, (int)sizeof(*data));
	*ok = out != nullptr && strcmp(out, hash) == 0;
	explicit_bzero(data, sizeof(*data));
	xfree(data);
	return true;
}

static bool bcrypt_cost(const char *hash, unsigned *cost, struct err **err) {
	// $2a$10$...
	struct str rest = str_from(hash), part;
	struct str parts[3];
	size_t n = 0;
	while (str_next_field(&rest, '$', &part)) {
		if (n < 3) {
			parts[n] = part;
		}
		n++;
	}
	if (n < 4) {
		return err_set(err, 0, "invalid password cost: malformed hash");
	}
	unsigned long long c = 0;
	if (!str_parse_u64(parts[2], &c) || c > UINT32_MAX) {
		return err_set(err, 0, "invalid password cost: invalid digit found in string");
	}
	*cost = (unsigned)c;
	return true;
}

bool db_user_check_password(struct db_user *u, const char *password, bool *upgraded,
			    struct err **err) {
	*upgraded = false;
	if (u->password[0] == '\0') {
		return err_set(err, 0, "password auth disabled");
	}
	bool ok;
	struct err *e = nullptr;
	if (!bcrypt_verify(password, u->password, &ok, &e)) {
		err_set(err, 0, "wrong password: %s", err_msg(e));
		err_free(e);
		return false;
	}
	if (!ok) {
		return err_set(
			err, 0,
			"wrong password: crypto/bcrypt: hashedPassword is not the hash of the "
			"given password");
	}
	unsigned cost = 0;
	if (!bcrypt_cost(u->password, &cost, err)) {
		return false;
	}
	if (cost < BCRYPT_DEFAULT_COST) {
		if (!db_user_set_password(u, password, err)) {
			return false;
		}
		*upgraded = true;
	}
	return true;
}

bool db_user_set_password(struct db_user *u, const char *password, struct err **err) {
	// Go's bcrypt rejects passwords longer than 72 bytes
	if (strlen(password) > 72) {
		return err_set(err, 0,
			       "failed to hash password: bcrypt: password length exceeds 72 bytes");
	}
	char salt[CRYPT_GENSALT_OUTPUT_SIZE];
	// Go's bcrypt uses the "$2a$" prefix
	if (crypt_gensalt_rn("$2a$", BCRYPT_DEFAULT_COST, nullptr, 0, salt, sizeof(salt)) ==
	    nullptr) {
		return err_set(err, 0, "failed to hash password: Rand error: crypt_gensalt failed");
	}
	struct crypt_data *data = xcalloc(1, sizeof(*data));
	const char *hash = crypt_rn(password, salt, data, (int)sizeof(*data));
	bool ok = hash != nullptr && hash[0] != '*';
	if (ok) {
		char *h = xstrdup(hash);
		free_secret(u->password);
		u->password = h;
	}
	explicit_bzero(data, sizeof(*data));
	xfree(data);
	if (!ok) {
		return err_set(err, 0, "failed to hash password: crypt failed");
	}
	return true;
}
