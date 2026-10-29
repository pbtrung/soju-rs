// Web Push delivery (soju-rs webpush.rs Sender): encrypted notifications
// POSTed to push services, never to internal addresses
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "http/client.h"
#include "net/url.h"
#include "webpush/webpush.h"

// soju-rs's push client timeout, and the TTL of notifications (a week)
static constexpr uint64_t WEBPUSH_TIMEOUT_MS = 15000;
static constexpr unsigned WEBPUSH_TTL_SECS = 7 * 24 * 60 * 60;

void webpush_sender_free(struct webpush_sender *s) {
	if (s == nullptr) {
		return;
	}
	xfree(s->vapid_public);
	if (s->vapid_private != nullptr) {
		explicit_bzero(s->vapid_private, strlen(s->vapid_private));
	}
	xfree(s->vapid_private);
	xfree(s);
}

struct send_job {
	webpush_done_fn done;
	void *data;
};

static void sent(void *data, const struct http_response *resp, const struct err *err) {
	struct send_job *j = data;
	if (resp == nullptr) {
		j->done(j->data, WEBPUSH_FAILED, err_msg(err));
	} else if (webpush_is_expired_status(resp->status)) {
		j->done(j->data, WEBPUSH_EXPIRED, nullptr);
	} else if (resp->status < 200 || resp->status > 299) {
		AUTO_STRBUF struct strbuf msg = STRBUF_INIT;
		strbuf_append_cstr(&msg, "HTTP error: ");
		http_status_text(&msg, resp->status);
		j->done(j->data, WEBPUSH_FAILED, strbuf_cstr(&msg));
	} else {
		j->done(j->data, WEBPUSH_SENT, nullptr);
	}
	xfree(j);
}

void webpush_send(const struct webpush_sender *s, uv_loop_t *loop, const struct logger *log,
		  const char *endpoint, const char *auth, const char *p256dh,
		  const char *vapid_pub_key, struct str payload, enum webpush_urgency urgency,
		  webpush_done_fn done, void *data) {
	struct err *e = nullptr;
	if (!cstr_eq(vapid_pub_key, s->vapid_public)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(vapid_pub_key));
		AUTOFREE char *msg = xasprintf("unknown VAPID public key %s", q);
		done(data, WEBPUSH_FAILED, msg);
		return;
	}
	struct url u;
	if (!url_parse(str_from(endpoint), &u, &e)) {
		done(data, WEBPUSH_FAILED, err_msg(e));
		err_free(e);
		return;
	}
	url_free(&u);
	struct http_client_request req;
	http_client_request_init(&req, "POST", endpoint);
	if (!http_check_external_url(endpoint, &e) ||
	    !webpush_encrypt(payload, auth, p256dh, &req.body, &e)) {
		done(data, WEBPUSH_FAILED, err_msg(e));
		err_free(e);
		http_client_request_free(&req);
		return;
	}
	AUTO_STRBUF struct strbuf authorization = STRBUF_INIT;
	if (!webpush_vapid_authorization(endpoint, "https://soju.im", s->vapid_public,
					 s->vapid_private, &authorization, &e)) {
		done(data, WEBPUSH_FAILED, err_msg(e));
		err_free(e);
		http_client_request_free(&req);
		return;
	}
	http_client_request_header(&req, "Content-Encoding", "aes128gcm");
	http_client_request_header(&req, "Content-Type", "application/octet-stream");
	http_client_request_header(&req, "TTL", "%u", WEBPUSH_TTL_SECS);
	http_client_request_header(&req, "Urgency", "%s",
				   urgency == WEBPUSH_HIGH ? "high" : "normal");
	http_client_request_header(&req, "Authorization", "%s", strbuf_cstr(&authorization));
	http_client_request_header(&req, "User-Agent", "soju");
	req.timeout_ms = WEBPUSH_TIMEOUT_MS;
	req.external_only = true;
	req.log = log;
	struct send_job *j = xmalloc(sizeof(*j));
	*j = (struct send_job){.done = done, .data = data};
	http_client_send(loop, &req, sent, j);
	http_client_request_free(&req);
}
