// CLIENTCERT (soju.im/client-cert) from registered downstream connections
// (soju-rs downstream/cmds/account.rs handle_clientcert)
#include "user/internal.h"

#include <ctype.h>
#include <openssl/evp.h>

#include "base/alloc.h"

struct cert_job {
	int64_t user;
	enum { CERT_LIST, CERT_STORE, CERT_DELETE } op;
	struct db_device_certificate cert;
	db_cert_vec certs;
	struct err *err;
};
static void cert_job(struct db *db, void *data) {
	struct cert_job *j = data;
	bool ok = j->op == CERT_LIST ? db_list_device_certificates(db, j->user, &j->certs, &j->err)
		  : j->op == CERT_STORE
			  ? db_store_device_certificate(db, j->user, &j->cert, &j->err)
			  : db_delete_device_certificate(db, j->user,
							 strbuf_str(&j->cert.fingerprint), &j->err);
	(void)ok;
}

static char *hex_upper(struct str bytes) {
	struct strbuf out = STRBUF_INIT;
	for (size_t i = 0; i < bytes.len; i++) {
		strbuf_printf(&out, "%02X", (unsigned char)bytes.p[i]);
	}
	return strbuf_take(&out);
}

static int hex_digit(char c) {
	return c >= '0' && c <= '9'   ? c - '0'
	       : c >= 'a' && c <= 'f' ? c - 'a' + 10
	       : c >= 'A' && c <= 'F' ? c - 'A' + 10
				      : -1;
}

static bool parse_hex(const char *s, struct strbuf *out) {
	size_t n = strlen(s);
	if (n % 2 != 0) {
		return false;
	}
	for (size_t i = 0; i < n; i += 2) {
		int hi = hex_digit(s[i]), lo = hex_digit(s[i + 1]);
		if (hi < 0 || lo < 0) {
			return false;
		}
		strbuf_append_char(out, (char)(hi * 16 + lo));
	}
	return true;
}

// The SHA-512 hash of the client's certificate, false without one
static bool peer_fingerprint(const struct user_client *dc, struct strbuf *out) {
	if (dc->peer_cert.len == 0) {
		return false;
	}
	unsigned char digest[EVP_MAX_MD_SIZE];
	unsigned len = 0;
	if (EVP_Digest(dc->peer_cert.buf, dc->peer_cert.len, digest, &len, EVP_sha512(), nullptr) !=
	    1) {
		return false;
	}
	strbuf_append(out, (const char *)digest, len);
	return true;
}

static void fail(struct user_packet *p, const char *code, const char *sub, const char *text) {
	user_packet_add_take(p, IRC_MSG("FAIL", "CLIENTCERT", code, sub, text));
}

void user_clientcert(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p) {
	struct server_config *cfg = server_config_get(u->srv);
	bool enabled = cfg->cfg.basic.client_cert_auth;
	server_config_unref(cfg);
	if (!enabled) {
		user_packet_add_take(p, IRC_MSG("421", "*", msg->command, "Unknown command"));
		return;
	}
	if (msg->params.len < 1) {
		user_packet_add_take(p, IRC_MSG("461", "*", msg->command, "Not enough parameters"));
		return;
	}
	AUTO_STRBUF struct strbuf upper = STRBUF_INIT;
	for (const char *c = irc_msg_param(msg, 0); *c != '\0'; c++) {
		strbuf_append_char(&upper, (char)toupper((unsigned char)*c));
	}
	const char *sub = strbuf_cstr(&upper);
	struct cert_job j = {.user = u->record.id};
	db_device_certificate_init(&j.cert);
	if (cstr_eq(sub, "LIST")) {
		db_thread_call(u->srv->db, cert_job, &j);
		if (j.err != nullptr) {
			log_info(&u->srv->log, "failed listing device certificates: %s",
				 err_msg(j.err));
			fail(p, "INTERNAL_ERROR", "LIST", "Internal error");
		}
		p->batch_type = xstrdup("soju.im/client-cert");
		for (size_t i = 0; i < j.certs.len; i++) {
			struct db_device_certificate *cert = &j.certs.data[i];
			AUTOFREE char *time = cert->last_used.set
						      ? format_server_time_cstr(cert->last_used.t)
						      : xstrdup("0001-01-01T00:00:00.000Z");
			struct irc_tags attrs = IRC_TAGS_INIT;
			irc_tags_set(&attrs, "time", time);
			irc_tags_set(&attrs, "ip", cert->last_ip);
			irc_tags_set(&attrs, "name", cert->label);
			AUTO_STRBUF struct strbuf text = STRBUF_INIT;
			irc_tags_format(&attrs, &text);
			irc_tags_free(&attrs);
			AUTOFREE char *fp = hex_upper(strbuf_str(&cert->fingerprint));
			struct irc_msg *m = IRC_MSG("CLIENTCERT", "LIST", fp, strbuf_cstr(&text));
			irc_msg_set_prefix(m, "*", "", "");
			user_packet_add_take(p, m);
		}
	} else if (cstr_eq(sub, "CREATE")) {
		if (msg->params.len < 2) {
			user_packet_add_take(
				p, IRC_MSG("461", "*", msg->command, "Not enough parameters"));
		} else if (!peer_fingerprint(dc, &j.cert.fingerprint)) {
			fail(p, "NOCERT", "CREATE", "No TLS client certificate is used");
		} else {
			xfree(j.cert.label);
			j.cert.label = xstrdup(irc_msg_param(msg, 1));
			AUTO_STRBUF struct strbuf ip = STRBUF_INIT;
			if (dc->has_remote_ip) {
				ipaddr_format(&ip, dc->remote_ip);
			}
			db_device_certificate_mark_used(&j.cert, strbuf_cstr(&ip));
			j.op = CERT_STORE;
			db_thread_call(u->srv->db, cert_job, &j);
			if (j.err != nullptr &&
			    j.err->code != DB_ERR_DUPLICATE_DEVICE_CERTIFICATE) {
				log_info(&u->srv->log, "failed creating device certificate: %s",
					 err_msg(j.err));
				fail(p, "INTERNAL_ERROR", "CREATE",
				     "Could not create device certificate");
			} else {
				struct irc_msg *m = IRC_MSG("CLIENTCERT", "CREATE");
				irc_msg_set_prefix(m, "*", "", "");
				user_packet_add_take(p, m);
			}
		}
	} else if (cstr_eq(sub, "DELETE")) {
		AUTOFREE char *fp = nullptr;
		bool ok = true;
		if (msg->params.len > 1) {
			fp = xstrdup(irc_msg_param(msg, 1));
			if (!parse_hex(fp, &j.cert.fingerprint) || j.cert.fingerprint.len != 64) {
				fail(p, "INTERNAL_ERROR", "DELETE",
				     "Invalid device certificate fingerprint");
				ok = false;
			}
		} else if (peer_fingerprint(dc, &j.cert.fingerprint)) {
			fp = hex_upper(strbuf_str(&j.cert.fingerprint));
		} else {
			fail(p, "NOCERT", "DELETE", "No TLS client certificate is used");
			ok = false;
		}
		if (ok) {
			j.op = CERT_DELETE;
			db_thread_call(u->srv->db, cert_job, &j);
			if (j.err != nullptr) {
				log_info(&u->srv->log, "failed deleting device certificate: %s",
					 err_msg(j.err));
				fail(p, "INTERNAL_ERROR", "DELETE",
				     "Could not delete device certificate");
			} else {
				struct irc_msg *m = IRC_MSG("CLIENTCERT", "DELETE", fp);
				irc_msg_set_prefix(m, "*", "", "");
				user_packet_add_take(p, m);
			}
		}
	} else {
		fail(p, "UNKNOWN_COMMAND", irc_msg_param(msg, 0), "Unknown subcommand");
	}
	err_free(j.err);
	db_device_certificate_free(&j.cert);
	db_cert_vec_free(&j.certs);
}
