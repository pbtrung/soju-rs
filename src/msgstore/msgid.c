#include "msgstore/msgid.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "msgstore/bare.h"

static constexpr uint64_t MSG_ID_VERSION = 0;

static char *format_msg_id(int64_t net_id, struct str target, uint64_t typ, uint64_t body) {
	AUTO_STRBUF struct strbuf w = STRBUF_INIT;
	bare_write_uint(&w, MSG_ID_VERSION);
	bare_write_int(&w, net_id);
	bare_write_string(&w, target);
	bare_write_uint(&w, typ);
	bare_write_uint(&w, body);
	return base64_encode_cstr(BASE64_URL_SAFE_NO_PAD, w.buf, w.len);
}

static bool wrap(struct err **err, struct err *e) {
	err_set(err, 0, "invalid internal message ID: %s", err_msg(e));
	err_free(e);
	return false;
}

bool parse_msg_id_with(struct str s, uint64_t typ, int64_t *net_id, struct strbuf *target,
		       struct strbuf *body, struct err **err) {
	AUTO_STRBUF struct strbuf b = STRBUF_INIT;
	struct err *e = nullptr;
	if (!base64_decode(&b, BASE64_URL_SAFE_NO_PAD, s, &e)) {
		return wrap(err, e);
	}
	struct str r = strbuf_str(&b);
	uint64_t version, got_typ;
	int64_t id;
	AUTO_STRBUF struct strbuf t = STRBUF_INIT;
	if (!bare_read_uint(&r, &version, &e) || !bare_read_int(&r, &id, &e) ||
	    !bare_read_string(&r, &t, &e) || !bare_read_uint(&r, &got_typ, &e)) {
		return wrap(err, e);
	}
	if (version != MSG_ID_VERSION) {
		return err_set(err, 0, "invalid internal message ID: got version %llu, want %llu",
			       (unsigned long long)version, (unsigned long long)MSG_ID_VERSION);
	}
	if (typ != MSG_ID_NONE && got_typ != typ) {
		return err_set(err, 0, "invalid internal message ID: got type %llu, want %llu",
			       (unsigned long long)got_typ, (unsigned long long)typ);
	}
	*net_id = id;
	if (target != nullptr) {
		strbuf_append_str(target, strbuf_str(&t));
	}
	if (body != nullptr) {
		strbuf_append_str(body, r);
	}
	return true;
}

bool parse_msg_id(struct str s, int64_t *net_id, struct strbuf *target, struct err **err) {
	return parse_msg_id_with(s, MSG_ID_NONE, net_id, target, nullptr, err);
}

char *format_memory_msg_id(int64_t net_id, struct str entity, uint64_t seq) {
	return format_msg_id(net_id, entity, MSG_ID_MEMORY, seq);
}

static bool parse_body_uint(struct str s, uint64_t typ, uint64_t *out, struct err **err) {
	int64_t net_id;
	AUTO_STRBUF struct strbuf body = STRBUF_INIT;
	if (!parse_msg_id_with(s, typ, &net_id, nullptr, &body, err)) {
		return false;
	}
	struct str r = strbuf_str(&body);
	struct err *e = nullptr;
	if (!bare_read_uint(&r, out, &e)) {
		return wrap(err, e);
	}
	return true;
}

bool parse_memory_msg_id(struct str s, uint64_t *seq, struct err **err) {
	return parse_body_uint(s, MSG_ID_MEMORY, seq, err);
}

char *format_db_msg_id(int64_t net_id, struct str target, int64_t msg_id) {
	return format_msg_id(net_id, target, MSG_ID_DB, (uint64_t)msg_id);
}

bool parse_db_msg_id(struct str s, int64_t *msg_id, struct err **err) {
	uint64_t u;
	if (!parse_body_uint(s, MSG_ID_DB, &u, err)) {
		return false;
	}
	*msg_id = (int64_t)u;
	return true;
}
