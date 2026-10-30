// Upstream IRC session and reconnect runner. Owned by one user loop.
// Network I/O, timers and queued user events use the session ID; a missing
// session discards the completion. Callbacks run on this loop and must not
// destroy the session synchronously.
#pragma once

#include "db/db.h"
#include "irc/casemapping.h"
#include "irc/ircutil.h"
#include "irc/multiline.h"
#include "net/conn.h"
#include "server/server.h"

static constexpr uint64_t UPSTREAM_REGISTER_TIMEOUT_MS = 60000;
static constexpr uint64_t UPSTREAM_CONNECT_TIMEOUT_MS = 15000;
static constexpr size_t MAX_PENDING_COMMANDS = 64;

struct upstream;
struct upstream_events {
	// Owned messages. send replaces the transport in protocol tests.
	void (*send)(struct upstream *uc, struct irc_msg *msg, void *data);
	void (*message)(struct upstream *uc, struct irc_msg *msg, uint64_t downstream_id,
			void *data);
	void (*state)(struct upstream *uc, void *data);
	void (*closed)(struct upstream *uc, const char *reason, bool temporary, void *data);
	void (*detach)(struct upstream *uc, const char *channel, void *data);
	// A joined channel's NAMES ended: its state is complete
	void (*channel_complete)(struct upstream *uc, const char *channel, void *data);
	void *data;
};

struct upstream_channel {
	char *name, *topic, *topic_who, *creation_time;
	struct db_time topic_time;
	enum channel_status status;
	struct channel_modes modes;
	struct cmmap members; // membership_set *
	bool complete;
	uint64_t detach_after_ms, detach_deadline;
};

struct upstream_user {
	char *nickname, *username, *hostname, *server, *flags, *account, *realname;
};

struct pending_command {
	uint64_t downstream_id, sent_at;
	char *label;
	struct irc_msg *msg;
};
typedef VEC(struct pending_command) pending_command_vec;

struct upstream_batch {
	uint64_t downstream_id;
	char *label;
	struct multiline_batch *multiline;
};

enum upstream_state {
	UPSTREAM_DISCONNECTED,
	UPSTREAM_CONNECTING,
	UPSTREAM_REGISTERING,
	UPSTREAM_ONLINE,
	UPSTREAM_STOPPED,
};

struct upstream {
	struct ref_node ref;
	uint64_t id;
	struct server *srv;
	struct sloop *loop;
	struct db_user user;
	struct db_network network;
	struct logger log;
	struct conn *conn;
	char *ident_remote, *ident_local;
	struct upstream_events events;
	uv_timer_t timer;
	struct cap_registry caps;
	struct isupport isupport;
	struct cmmap channels, users, monitored;
	struct strmap batches, pending;
	struct channel_mode_types mode_types;
	VEC(struct membership) memberships;
	struct user_modes modes;
	enum casemapping casemap;
	enum upstream_state state;
	char *nick, *username, *hostname, *realname, *account;
	char *channel_types, *status_msg, *server_name;
	bool registered, attached, got_motd, sasl_active, sasl_started;
	bool rate_enabled, closing, reconnect, away;
	uint64_t now, deadline, retry_at, regain_at, next_label, next_batch;
	uint64_t generation;
	struct backoffer reconnect_backoff, regain_backoff;
	size_t connect_command;
	uint64_t command_at, attach_at;
	irc_msg_vec delayed;
	char *ended_command, *ended_target;
	uint64_t ended_downstream;
};

[[nodiscard]] struct upstream *upstream_new(struct server *srv, const struct db_user *u,
					    const struct db_network *n,
					    const struct upstream_events *events);
void upstream_free(struct upstream *uc);
[[nodiscard]] struct upstream *upstream_lookup(uint64_t id);
// Start/restart the runner; failed attempts use exponential backoff/jitter.
void upstream_connect(struct upstream *uc);
// Adopt a connected transport (takes its reference); registration starts on
// the ready callback for outgoing sockets, or immediately for adopted ones.
void upstream_adopt(struct upstream *uc, struct conn *conn);
void upstream_register(struct upstream *uc);
void upstream_receive_take(struct upstream *uc, struct irc_msg *msg);
void upstream_send_take(struct upstream *uc, struct irc_msg *msg);
void upstream_send_labeled_take(struct upstream *uc, uint64_t dc_id, const char *label,
				struct irc_msg *msg);
// Takes msg on both success and failure. Failure sends an abort reply via events.message.
[[nodiscard]] bool upstream_enqueue_take(struct upstream *uc, uint64_t dc_id, const char *label,
					 struct irc_msg *msg);
void upstream_cancel_downstream(struct upstream *uc, uint64_t dc_id);
void upstream_tick(struct upstream *uc, uint64_t now_ms);
void upstream_set_auto_detach(struct upstream *uc, const char *channel, uint64_t delay_ms);
void upstream_set_casemapping(struct upstream *uc, enum casemapping cm);
[[nodiscard]] bool upstream_is_channel(const struct upstream *uc, const char *name);
[[nodiscard]] bool upstream_supports_sasl(const struct upstream *uc, const char *mechanism);
void upstream_caps_to_request(const struct cap_registry *caps, strvec *out);
[[nodiscard]] bool upstream_parse_label(const char *label, uint64_t *dc_id, char **out,
					struct err **err);
[[nodiscard]] bool upstream_registration_temporary(const struct irc_msg *msg);
[[nodiscard]] char *upstream_registration_error(const struct irc_msg *msg);
[[nodiscard]] bool upstream_user_has_whox_fields(const struct upstream_user *u, const char *fields);
[[nodiscard]] char *go_errno_message(int uv_error);
