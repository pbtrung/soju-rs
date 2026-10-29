#include "service/internal.h"
#include "net/certfp.h"
#include <openssl/crypto.h>

struct sasl_job {
	int64_t user;
	struct db_network record;
	struct err *err;
};
static void sasl_store(struct db *db, void *data) {
	struct sasl_job *j = data;
	bool ok = db_store_network(db, j->user, &j->record, &j->err);
	(void)ok;
}
static bool store_sasl(struct user *u, struct user_network *net, struct db_network *record,
		       struct service_context *c) {
	struct sasl_job j = {.user = u->record.id};
	db_network_copy(&j.record, record);
	db_thread_call(u->srv->db, sasl_store, &j);
	if (j.err != nullptr) {
		db_network_free(&j.record);
		c->err = j.err;
		return false;
	}
	db_network_free(&net->record);
	net->record = j.record;
	// Credentials are used on the next registration; updating them must not
	// disrupt the current connection or throw away its channel state.
	if (net->upstream != nullptr) {
		db_network_free(&net->upstream->network);
		db_network_copy(&net->upstream->network, &net->record);
	}
	return true;
}
static struct user_network *parse_network(struct user *u, struct service_context *c,
					  struct flags_parsed *p, const char *spec,
					  const char *const *args, size_t n, bool no_args) {
	if (!service_parse(c, p, spec, args, n) || (no_args && !service_no_args(c, p))) {
		return nullptr;
	}
	return service_network(u, c, flags_get(p, "network"));
}
static void fingerprints(struct service_context *c, struct str cert) {
	const char *const hashes[] = {"SHA1", "SHA256", "SHA512"};
	const char *const labels[] = {"SHA-1", "SHA-256", "SHA-512"};
	for (size_t i = 0; i < 3; i++) {
		AUTOFREE char *fp = service_digest(hashes[i], cert);
		service_printf(c, "%s fingerprint: %s", labels[i], fp);
	}
}
struct cert_job {
	struct service_context *c;
	int64_t network;
	char *type;
	unsigned bits;
	struct strbuf key, cert;
	struct err *err;
};
static void cert_work(void *data) {
	struct cert_job *j = data;
	bool ok = generate_certfp(j->type, j->bits, &j->key, &j->cert, &j->err);
	(void)ok;
	// libuv workers survive until process exit, beyond OpenSSL global cleanup.
	// Release this job's per-thread crypto state while its providers are alive.
	OPENSSL_thread_stop();
}
static void cert_free(struct cert_job *j) {
	xfree(j->type);
	strbuf_free(&j->key);
	strbuf_free(&j->cert);
	err_free(j->err);
	xfree(j);
}
static void cert_drop(void *data) {
	struct cert_job *j = data;
	service_drop(j->c);
	cert_free(j);
}
static void cert_apply(struct user *u, void *data) {
	struct cert_job *j = data;
	struct service_context *c = j->c;
	u->certfp_pending = false;
	struct user_network *net = user_get_network(u, j->network);
	if (net == nullptr) {
		bool ok = SFAIL(c, "network was deleted");
		(void)ok;
	} else if (uv_hrtime() > c->deadline) {
		bool ok = SFAIL(c, "context deadline exceeded");
		(void)ok;
	} else {
		struct db_network r;
		db_network_copy(&r, &net->record);
		service_replace(&r.sasl.mechanism, "EXTERNAL");
		strbuf_free(&r.sasl.external_cert.data);
		r.sasl.external_cert = (struct db_blob){.set = true};
		strbuf_append_str(&r.sasl.external_cert.data, strbuf_str(&j->cert));
		strbuf_free(&r.sasl.external_key.data);
		r.sasl.external_key = (struct db_blob){.set = true};
		strbuf_append_str(&r.sasl.external_key.data, strbuf_str(&j->key));
		if (store_sasl(u, net, &r, c)) {
			service_print(c, "certificate generated");
			fingerprints(c, strbuf_str(&j->cert));
		}
		db_network_free(&r);
	}
	service_finish(c);
	cert_free(j);
}
static void cert_error(struct user *u, void *data) {
	struct cert_job *j = data;
	u->certfp_pending = false;
	service_finish(j->c);
	cert_free(j);
}
static void cert_done(void *data, const char *err) {
	struct cert_job *j = data;
	if (err != nullptr) {
		bool ok = SFAIL(j->c, "%s", err);
		(void)ok;
	} else if (j->err != nullptr) {
		j->c->err = j->err;
		j->err = nullptr;
	}
	user_post(j->c->srv, j->c->user_id, j->c->err == nullptr ? cert_apply : cert_error,
		  cert_drop, j);
}
HANDLER(certfp_generate) {
	struct flags_parsed p = {0};
	struct user_network *net =
		parse_network(u, c, &p, "network:s key-type:s bits:i", args, n, true);
	bool ok = net != nullptr;
	long long bits = 3072;
	bool present = flags_get_int(&p, "bits", &bits);
	(void)present;
	if (ok && (bits <= 0 || bits > 8192)) {
		ok = SFAIL(c, "invalid value for -bits");
	}
	if (ok && u->certfp_pending) {
		ok = SFAIL(c, "a certificate is already being generated");
	}
	if (ok) {
		struct cert_job *j = xcalloc(1, sizeof(*j));
		*j = (struct cert_job){.c = c,
				       .network = net->record.id,
				       .type = xstrdup(service_flag_string(&p, "key-type", "rsa")),
				       .bits = (unsigned)bits};
		u->certfp_pending = true;
		service_defer(c);
		server_cpu(c->srv, cert_work, cert_done, j);
	}
	flags_parsed_free(&p);
	return ok ? SERVICE_DEFERRED : false;
}
HANDLER(certfp_fingerprints) {
	struct flags_parsed p = {0};
	struct user_network *net = parse_network(u, c, &p, "network:s", args, n, true);
	bool ok = net != nullptr;
	if (ok && !cstr_eq(net->record.sasl.mechanism, "EXTERNAL")) {
		ok = SFAIL(c, "CertFP not set up");
	}
	if (ok) {
		fingerprints(c, strbuf_str(&net->record.sasl.external_cert.data));
	}
	flags_parsed_free(&p);
	return ok;
}
HANDLER(sasl_status) {
	struct flags_parsed p = {0};
	struct user_network *net = parse_network(u, c, &p, "network:s", args, n, true);
	if (net != nullptr) {
		struct db_sasl *s = &net->record.sasl;
		if (cstr_eq(s->mechanism, "PLAIN")) {
			service_printf(c, "SASL PLAIN enabled with username \"%s\"",
				       s->plain_username);
		} else if (cstr_eq(s->mechanism, "EXTERNAL")) {
			service_print(c, "SASL EXTERNAL (CertFP) enabled");
		} else if (s->mechanism[0] == '\0') {
			service_print(c, "SASL is disabled");
		}
		struct upstream *uc = net->upstream;
		if (uc == nullptr || !uc->attached) {
			service_print(c, "Disconnected from upstream network");
		} else if (uc->account[0]) {
			service_printf(c, "Authenticated on upstream network with account \"%s\"",
				       uc->account);
		} else {
			service_print(c, "Unauthenticated on upstream network");
		}
	}
	flags_parsed_free(&p);
	return net != nullptr;
}
static bool sasl_set(struct user *u, struct service_context *c, const char *const *args, size_t n,
		     bool reset) {
	struct flags_parsed p = {0};
	struct user_network *net = parse_network(u, c, &p, "network:s", args, n, false);
	bool ok = net != nullptr;
	if (ok && p.args.len != (reset ? 0U : 2U)) {
		ok = SFAIL(c, reset ? "expected no argument" : "expected exactly 2 arguments");
	}
	if (ok) {
		struct db_network r;
		db_network_copy(&r, &net->record);
		service_replace(&r.sasl.mechanism, reset ? "" : "PLAIN");
		service_replace(&r.sasl.plain_username, reset ? "" : p.args.data[0]);
		service_replace(&r.sasl.plain_password, reset ? "" : p.args.data[1]);
		if (reset) {
			strbuf_free(&r.sasl.external_cert.data);
			r.sasl.external_cert.set = false;
			strbuf_free(&r.sasl.external_key.data);
			r.sasl.external_key.set = false;
		}
		ok = store_sasl(u, net, &r, c);
		db_network_free(&r);
		if (ok) {
			service_print(c, reset ? "credentials reset" : "credentials saved");
		}
	}
	flags_parsed_free(&p);
	return ok;
}
HANDLER(sasl_set_plain) {
	return sasl_set(u, c, args, n, false);
}
HANDLER(sasl_reset) {
	return sasl_set(u, c, args, n, true);
}

struct device_job {
	int64_t user;
	enum { DEVICE_LIST, DEVICE_STORE, DEVICE_DELETE } op;
	struct db_device_certificate cert;
	db_cert_vec certs;
	struct err *err;
};
static void device_db(struct db *db, void *data) {
	struct device_job *j = data;
	bool ok = j->op == DEVICE_LIST
			  ? db_list_device_certificates(db, j->user, &j->certs, &j->err)
		  : j->op == DEVICE_STORE
			  ? db_store_device_certificate(db, j->user, &j->cert, &j->err)
			  : db_delete_device_certificate(db, j->user,
							 strbuf_str(&j->cert.fingerprint), &j->err);
	(void)ok;
}
HANDLER(device_cert_status) {
	(void)args;
	if (n) {
		return SFAIL(c, "expected no argument");
	}
	struct device_job j = {.user = u->record.id};
	db_thread_call(c->srv->db, device_db, &j);
	if (j.err != nullptr) {
		c->err = j.err;
		return false;
	}
	if (j.certs.len == 0) {
		service_print(c, "No device certificate configured.");
	}
	for (size_t i = 0; i < j.certs.len; i++) {
		struct db_device_certificate *cert = &j.certs.data[i];
		struct str fp = strbuf_str(&cert->fingerprint);
		fp.len = fp.len < 6 ? fp.len : 6;
		AUTOFREE char *hex = service_hex(fp, true);
		struct civil t = timestamp_to_civil(
			cert->last_used.set ? cert->last_used.t
					    : timestamp_from_unix(-62135596800LL, 0));
		service_printf(c, "%s [%s] [%s] [%04lld-%02u-%02u %02u:%02u:%02u]", hex,
			       cert->last_ip, cert->label, (long long)t.year, t.month, t.day,
			       t.hour, t.minute, t.second);
	}
	db_cert_vec_free(&j.certs);
	return true;
}
HANDLER(device_cert_create) {
	struct server_config *cfg = server_config_get(c->srv);
	bool enabled = cfg->cfg.basic.client_cert_auth;
	server_config_unref(cfg);
	if (!enabled) {
		return SFAIL(c, "client certification authentication is disabled");
	}
	struct flags_parsed p = {0};
	bool ok = service_parse(c, &p, "fingerprint:s label:s", args, n) && service_no_args(c, &p);
	const char *fp = flags_get(&p, "fingerprint");
	AUTOFREE char *derived = nullptr;
	if ((fp == nullptr || fp[0] == '\0') && c->peer_cert.len) {
		derived = service_digest("SHA512", strbuf_str(&c->peer_cert));
		fp = derived;
	}
	const char *label = flags_get(&p, "label");
	struct device_job j = {.user = u->record.id, .op = DEVICE_STORE};
	db_device_certificate_init(&j.cert);
	if (ok && (fp == nullptr || fp[0] == '\0')) {
		ok = SFAIL(c,
			   "flag -fingerprint is required when no TLS client certificate is used");
	}
	if (ok && (label == nullptr || label[0] == '\0')) {
		ok = SFAIL(c, "flag -label is required");
	}
	if (ok && (!service_unhex(fp, &j.cert.fingerprint) || j.cert.fingerprint.len != 64)) {
		ok = SFAIL(c, "the certificate fingerprint must be hex-encoded SHA-512");
	}
	if (ok) {
		service_replace(&j.cert.label, label);
		AUTO_STRBUF struct strbuf ip = {0};
		if (c->has_remote_ip) {
			ipaddr_format(&ip, c->remote_ip);
		}
		db_device_certificate_mark_used(&j.cert, strbuf_cstr(&ip));
		db_thread_call(c->srv->db, device_db, &j);
		if (j.err != nullptr) {
			if (j.err->code == DB_ERR_DUPLICATE_DEVICE_CERTIFICATE) {
				err_free(j.err);
				ok = SFAIL(c, "this device certificate fingerprint already exists");
			} else {
				c->err = j.err;
				ok = false;
			}
		} else {
			service_printf(c, "created device certificate \"%s\"", fp);
		}
	}
	db_device_certificate_free(&j.cert);
	flags_parsed_free(&p);
	return ok;
}
HANDLER(device_cert_delete) {
	if (n != 1) {
		return SFAIL(c, "expected exactly one argument");
	}
	AUTO_STRBUF struct strbuf prefix = {0};
	if (!service_unhex(args[0], &prefix) || prefix.len < 6 || prefix.len > 64) {
		return SFAIL(c, "the certificate fingerprint must be a hex-encoded SHA-512 hash, "
				"or a prefix of it");
	}
	struct device_job j = {.user = u->record.id};
	db_thread_call(c->srv->db, device_db, &j);
	if (j.err != nullptr) {
		c->err = j.err;
		return false;
	}
	struct db_device_certificate *found = nullptr;
	bool ok = true;
	for (size_t i = 0; i < j.certs.len; i++) {
		if (str_starts_with(strbuf_str(&j.certs.data[i].fingerprint),
				    strbuf_str(&prefix))) {
			if (found != nullptr) {
				ok = SFAIL(c, "multiple device certificates match this fingerprint "
					      "prefix; pass the full fingerprint");
				break;
			}
			found = &j.certs.data[i];
		}
	}
	if (ok && found == nullptr) {
		ok = SFAIL(c, "no device certificate found for fingerprint \"%s\"", args[0]);
	}
	if (ok) {
		db_device_certificate_init(&j.cert);
		strbuf_append_str(&j.cert.fingerprint, strbuf_str(&found->fingerprint));
		j.op = DEVICE_DELETE;
		db_thread_call(c->srv->db, device_db, &j);
		if (j.err != nullptr) {
			c->err = j.err;
			ok = false;
		} else {
			service_printf(c, "deleted device certificate \"%s\"", args[0]);
		}
		db_device_certificate_free(&j.cert);
	}
	db_cert_vec_free(&j.certs);
	return ok;
}
