// Web Push notifications (soju-rs webpush.rs): message encryption (RFC
// 8291, aes128gcm), VAPID authentication (RFC 8292) and delivery,
// compatible with github.com/SherClockHolmes/webpush-go
#pragma once

#include <stdint.h>
#include <uv.h>

#include "base/err.h"
#include "base/log.h"
#include "base/str.h"

static constexpr uint32_t WEBPUSH_RECORD_SIZE = 2048;

enum webpush_urgency {
	WEBPUSH_NORMAL,
	WEBPUSH_HIGH,
};

// Generates a VAPID key pair: private and public keys in unpadded base64url
void webpush_generate_vapid_keys(char **private_key, char **public_key);

// Encrypts a message for a subscription (auth and p256dh keys in base64),
// with a random salt and local key: one record padded to the record size
[[nodiscard]] bool webpush_encrypt(struct str message, const char *auth, const char *p256dh,
				   struct strbuf *out, struct err **err);

// The VAPID Authorization header for an endpoint ("vapid t=<JWT>, k=<key>")
[[nodiscard]] bool webpush_vapid_authorization(const char *endpoint, const char *subscriber,
					       const char *public_key, const char *private_key,
					       struct strbuf *out, struct err **err);

// Whether a push service status means the subscription is gone: 404 Not
// Found or 410 Gone (RFC 8030 section 7.3)
[[nodiscard]] bool webpush_is_expired_status(int status);

// Sends notifications with a fixed VAPID key pair (immutable, shared)
struct webpush_sender {
	char *vapid_public;
	char *vapid_private;
};

void webpush_sender_free(struct webpush_sender *s);

enum webpush_result {
	WEBPUSH_SENT,
	WEBPUSH_EXPIRED, // the subscription is gone
	WEBPUSH_FAILED,
};

// err is set for WEBPUSH_FAILED
typedef void (*webpush_done_fn)(void *data, enum webpush_result result, const char *err);

// Sends a notification on the current loop; done may be called before
// this returns (invalid keys or endpoint)
void webpush_send(const struct webpush_sender *s, uv_loop_t *loop, const struct logger *log,
		  const char *endpoint, const char *auth, const char *p256dh,
		  const char *vapid_pub_key, struct str payload, enum webpush_urgency urgency,
		  webpush_done_fn done, void *data);
