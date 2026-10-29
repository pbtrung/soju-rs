// User actor and networks. Every mutable object here lives on user_id % N.
// Other loops post owned events addressed by user/downstream/session IDs.
#pragma once
#include "downstream/downstream.h"
#include "msgstore/msgstore.h"
#include "upstream/upstream.h"
#include "user/delivered.h"

struct user_client {
	struct server *srv;
	size_t queued, history_pending;
	uint64_t id, parent_id;
	int64_t network_id;
	struct sloop *loop;
	char *client, *nick, *username, *hostname, *realname, *away;
	struct cap_registry caps;
	struct cmmap monitored;
	struct strmap multiline;
	bool impersonating, has_remote_ip;
	struct ipaddr remote_ip;
	struct strbuf peer_cert;
};

struct user_network {
	struct db_network record;
	struct upstream *upstream;
	struct cmmap channels; // db_channel *
	struct delivered_store delivered;
	struct cmmap latest_times; // timestamp *
	char *last_error;
	uint64_t joined_generation;
};

struct user {
	struct ref_node ref;
	struct server *srv;
	struct sloop *loop;
	struct db_user record;
	VEC(struct user_network *) networks;
	VEC(struct user_client *) clients;
	struct msgstore store;
	bool stopped, certfp_pending;
};

// Local lookup only. No user pointer may cross a loop boundary.
[[nodiscard]] struct user *user_lookup(struct server *srv, int64_t id);
[[nodiscard]] struct user_client *user_get_client(struct user *u, uint64_t id);
[[nodiscard]] struct user_network *user_get_network(struct user *u, int64_t id);
[[nodiscard]] struct user_network *user_find_network(struct user *u, const char *name);
// Consumes an owned user record; duplicate starts preserve the actor.
void user_start_take(struct server *srv, struct db_user *record);
[[nodiscard]] bool user_server_start(struct server *srv, struct err **err);
void user_server_stop(struct server *srv);
// Stops and frees one actor on its owning loop.
void user_stop(struct user *u);
// Registration callback, and the registered message/disconnect callbacks.
void user_downstream_registered(struct downstream *dc, void *data);
void user_downstream_message(struct downstream *dc, struct irc_msg *msg, void *data);
void user_downstream_closed(struct downstream *dc, void *data);
void user_downstream_updated(struct downstream *dc, void *data);
// Posts work on a user; drop frees its data if the user disappeared.
// run owns data on success. Neither callback may touch another loop's state.
typedef void (*user_run_fn)(struct user *u, void *data);
void user_post(struct server *srv, int64_t id, user_run_fn run, loop_task_fn drop, void *data);
void user_broadcast_take(struct server *srv, struct irc_msg *msg);
void user_send_take(struct user *u, const struct user_client *dc, struct irc_msg *msg);
void user_service_reply(struct user *u, uint64_t dc_id, const char *text);
// Called only on this user's loop; records are copied, IDs returned on create.
[[nodiscard]] bool user_store_network(struct user *u, struct db_network *record, bool check_limit,
				      struct err **err);
[[nodiscard]] bool user_delete_network(struct user *u, int64_t id, struct err **err);
[[nodiscard]] bool user_store_channel(struct user *u, struct user_network *net,
				      struct db_channel *record, struct err **err);
void user_detach_channel(struct user *u, struct user_network *net, const char *channel);
void user_attach_channel(struct user *u, struct user_network *net, const char *channel);
void user_close_clients(struct user *u, const char *reason);
void user_notify_network(struct user *u, struct user_network *net);
[[nodiscard]] bool user_network_highlight(const struct user *u, const struct user_network *net,
					  const struct irc_msg *msg);
[[nodiscard]] char *user_ident(int64_t id);
[[nodiscard]] bool user_local_tcp_addr(const struct ipnet *nets, size_t n, int64_t user_id,
				       struct ipaddr remote, struct ipaddr *out, bool *found,
				       struct err **err);
