// Downstream connections from IRC clients (soju-rs downstream/*): state,
// capabilities, SASL, labels, batches, welcome and synchronization.
//
// A downstream belongs to its connection's loop. Asynchronous work
// (authentication) finds it again by ID (PLAN.md 2.1 rule 2).
#pragma once

#include <stdint.h>

#include "base/err.h"
#include "base/log.h"
#include "base/map.h"
#include "base/ref.h"
#include "irc/casemapping.h"
#include "irc/message.h"
#include "irc/multiline.h"
#include "irc/xirc.h"
#include "net/conn.h"
#include "net/listener.h"
#include "server/server.h"

static constexpr char SERVICE_NICK[] = "BouncerServ";
static constexpr char SERVICE_NICK_CM[] = "bouncerserv";
static constexpr char SERVICE_REALNAME[] = "soju bouncer service";
static constexpr size_t BACKLOG_LIMIT = 4000;

// Characters forbidden in nicknames and channel names
static constexpr char ILLEGAL_NICK_CHARS[] = " :@!*?$,.";
static constexpr char ILLEGAL_CHAN_CHARS[] = " ,\x07";

struct sasl_plain {
	char *identity, *username, *password;
};

struct oauth_bearer {
	char *username, *token;
};

enum sasl_kind {
	SASL_PLAIN,
	SASL_OAUTHBEARER,
	SASL_ANONYMOUS,
	SASL_EXTERNAL,
};

struct downstream_sasl {
	enum sasl_kind kind;
	char *mechanism;
	bool done;
	char *oauth_fail; // nullptr unless OAUTHBEARER failed
	bool has_plain;
	struct sasl_plain plain;
	bool has_oauth;
	struct oauth_bearer oauth;
	struct strbuf pending_resp;
};

void downstream_sasl_free(struct downstream_sasl *s);
// Feeds a client response (nullptr: none yet). On success, *challenge is
// set to the challenge to send back (owned bytes), or left nullptr when
// the exchange is complete.
[[nodiscard]] bool downstream_sasl_next(struct downstream_sasl *s, const struct str *response,
					struct strbuf **challenge, struct err **err);

struct registration {
	char *nick, *username, *pass, *network_name;
	int64_t network_id;
	bool negotiating_caps;
	char *auth_username;
	bool server_checked;
	unsigned auth_failures; // failed SASL attempts, see MAX_AUTH_FAILURES
};

struct label_ctx {
	char *label;
	char *batch;
	// The first reply, buffered until we know whether a batch is needed
	irc_msg_vec pending;
	bool has_pending;
};

struct downstream;

// What the registration hands over
struct downstream_events {
	// Registration is complete (dc->username is the authenticated user);
	// the callee takes over dc, or closes it
	void (*registered)(struct downstream *dc, void *data);
	// Registered input (takes msg). Call downstream_input_done after the
	// user's event has been handled, including when its target disappeared.
	void (*message)(struct downstream *dc, struct irc_msg *msg, void *data);
	void (*closed)(struct downstream *dc, void *data);
	void (*updated)(struct downstream *dc, void *data);
	void *data;
};

struct downstream {
	struct ref_node ref;
	uint64_t id;
	struct server *srv;
	struct conn *conn; // reference
	struct conn_view *view; // set for virtual connections (multiplex)
	struct logger log;
	struct downstream_events events;

	char *remote_addr;
	struct strbuf peer_cert;
	struct ipaddr remote_ip;

	int64_t network_id;
	uint64_t parent_id;
	char *client_name;

	char *nick, *nick_cm, *realname, *username, *hostname, *account;
	char *away; // nullptr: not away

	int64_t cap_version;
	struct cap_registry caps;
	struct isupport isupport;
	struct downstream_sasl *sasl;
	struct registration *reg; // nullptr after RPL_WELCOME

	uint64_t last_batch_ref;
	struct strmap multiline_batches; // reference -> struct multiline_batch *
	struct cmmap monitored;
	struct strmap metadata_subs;
	struct label_ctx *label;

	uv_timer_t register_timer;
	enum casemapping casemap;
	bool has_remote_ip;
	bool has_peer_cert;
	bool registered;
	bool has_network;
	bool has_parent; // a virtual connection: parent_id is its bouncer connection
	bool impersonating;
	bool busy; // an asynchronous registration step is running
	bool processing_input, waiting_input;
	irc_msg_vec pending_input;
	bool closed;
	int64_t user_id;
	struct sloop *user_loop; // routing only; no user object crosses loops
};

// Serves a new connection (takes the reference on c)
void downstream_serve(struct server *srv, struct conn *c, const struct conn_info *info,
		      const struct downstream_events *events);
// The downstream with this ID on the current loop, or nullptr
[[nodiscard]] struct downstream *downstream_lookup(uint64_t id);
void downstream_close(struct downstream *dc);
// Closes this server's downstreams on the current loop, including clients
// which have not authenticated yet.
void downstream_close_server(struct server *srv);
// Dispatches owned input, also used by multiplex and the session fuzzer.
void downstream_receive_take(struct downstream *dc, struct irc_msg *msg);
void downstream_input_done(struct downstream *dc);
// Completes an asynchronous registration/attachment step and drains the
// retained input in order. Its input permits bound the queue to 64 lines.
void downstream_resume_input(struct downstream *dc);
void downstream_begin_command(struct downstream *dc, const struct irc_msg *msg);
void downstream_end_command(struct downstream *dc);
void downstream_registration_done(struct downstream *dc);
// A virtual connection inherits capabilities and identity, with independent
// labels, batches and multiline state. It holds its own connection reference.
[[nodiscard]] struct downstream *downstream_attach(struct downstream *parent, int64_t network_id);
[[nodiscard]] struct downstream *downstream_find_attached(uint64_t parent_id, int64_t network_id);
void downstream_detach(struct downstream *dc);

// Borrowed state snapshot supplied by the user on the connection's loop.
struct downstream_sync {
	const char *nick, *username, *hostname, *realname, *account;
	const char *modes;
	enum casemapping casemap;
	bool admin, chathistory, has_network;
	int64_t network_id;
	const struct cap_registry *up_caps;
	const struct isupport *up_isupport;
};
void downstream_update_caps(struct downstream *dc, const struct downstream_sync *s);
void downstream_sync(struct downstream *dc, const struct downstream_sync *s);
void downstream_welcome(struct downstream *dc, const struct downstream_sync *s);

// Sending (soju-rs downstream/send.rs)
void downstream_send(struct downstream *dc, struct irc_msg *msg); // takes msg
void downstream_flush_batch(struct downstream *dc);
// Takes the label of the current command (a new string, "" if none)
[[nodiscard]] char *downstream_take_label(struct downstream *dc);
void downstream_drop_label(struct downstream *dc);
// Starts a batch if the client supports it; returns its reference (new)
[[nodiscard]] char *downstream_begin_batch(struct downstream *dc, const char *type,
					   const char *const *params, size_t n,
					   const struct irc_tags *tags);
void downstream_end_batch(struct downstream *dc, const char *ref);
// Sends messages in a batch (takes them)
void downstream_send_batch(struct downstream *dc, const char *type, const char *const *params,
			   size_t n, irc_msg_vec *msgs);
void downstream_send_message_with_id(struct downstream *dc, struct irc_msg *msg, const char *id);
void downstream_advance_message_with_id(struct downstream *dc, const struct irc_msg *msg,
					const char *id);
// Ends SASL with msg (taken), or RPL_SASLSUCCESS when nullptr
void downstream_end_sasl(struct downstream *dc, struct irc_msg *msg);
void downstream_send_service_notice(struct downstream *dc, const char *text);
void downstream_send_service_privmsg(struct downstream *dc, const char *text);

// Capabilities (soju-rs downstream/caps.rs)
void downstream_set_supported_cap(struct downstream *dc, const char *name, const char *value);
void downstream_unset_supported_cap(struct downstream *dc, const char *name);
void downstream_update_casemapping(struct downstream *dc, enum casemapping cm);

// The ISUPPORT context: up_isupport/up_caps are the upstream's, or nullptr
struct isupport_ctx {
	bool has_user;
	bool has_network;
	int64_t network_id;
	bool chathistory;
	const struct isupport *up_isupport;
	const struct cap_registry *up_caps;
};

void downstream_build_isupport(struct downstream *dc, const struct isupport_ctx *ctx,
			       struct isupport *out);
void downstream_send_isupport(struct downstream *dc, const struct isupport *is);

// Utilities (soju-rs downstream/mod.rs)
// "<username>[/<network>][@<client>]" in any order (new strings)
void unmarshal_username(const char *raw, char **username, char **client, char **network);
// NickServ REGISTER/IDENTIFY/SET PASSWORD: *account and *password new
// strings; false if the text isn't one of these
[[nodiscard]] bool parse_nickserv_credentials(const char *text, const char *nick, char **account,
					      char **password);
struct db_network;
void fill_network_addr_attrs(struct irc_tags *attrs, const struct db_network *n);
// Applies BOUNCER network attributes; on error, *irc is the FAIL reply
[[nodiscard]] bool update_network_attrs(struct db_network *n, const struct irc_tags *attrs,
					const char *subcommand, struct irc_msg **irc);
