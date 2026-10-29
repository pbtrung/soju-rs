// Persistent storage for users, networks, channels and messages: the local
// SQLite backend of soju-rs (database/mod.rs, database/sqlite/*). All
// functions are synchronous and run on the DB thread (db/dbthread.h); a
// struct db is used by one thread at a time.
//
// Records own their strings (never nullptr, "" for none) and are freed
// with their *_free function.
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "base/err.h"
#include "base/str.h"
#include "base/timeutil.h"
#include "base/vec.h"
#include "irc/message.h"

// bcrypt's default cost, as in golang.org/x/crypto/bcrypt
static constexpr unsigned BCRYPT_DEFAULT_COST = 10;
// Error code of db_store_device_certificate for an existing fingerprint
// (message "duplicate device certificate")
static constexpr int DB_ERR_DUPLICATE_DEVICE_CERTIFICATE = 1;

// An optional byte string
struct db_blob {
	bool set;
	struct strbuf data;
};

// An optional time
struct db_time {
	bool set;
	struct timestamp t;
};

struct db_user {
	int64_t id;
	char *username;
	char *password; // bcrypt hash, "" if password authentication is disabled
	char *nick;
	char *realname;
	bool admin;
	bool enabled;
	struct db_time downstream_interacted_at;
	int64_t max_networks;
};

// User::new
void db_user_init(struct db_user *u, const char *username);
void db_user_copy(struct db_user *dst, const struct db_user *src);
void db_user_free(struct db_user *u);
// Checks a password. Sets *upgraded if the hash was replaced by a stronger
// one and the record needs to be stored.
[[nodiscard]] bool db_user_check_password(struct db_user *u, const char *password, bool *upgraded,
					  struct err **err);
[[nodiscard]] bool db_user_set_password(struct db_user *u, const char *password, struct err **err);

struct db_sasl {
	char *mechanism;
	char *plain_username;
	char *plain_password;
	struct db_blob external_cert; // X.509 certificate, DER
	struct db_blob external_key; // PKCS#8 private key, DER
};

struct db_network {
	int64_t id;
	char *name;
	char *addr;
	char *nick;
	char *username;
	char *realname;
	char *pass;
	strvec connect_commands;
	char *certfp;
	struct db_sasl sasl;
	bool auto_away;
	bool enabled;
};

// Network::new
void db_network_init(struct db_network *n, const char *addr);
void db_network_copy(struct db_network *dst, const struct db_network *src);
void db_network_free(struct db_network *n);
// The name, or the address if it has none (borrowed)
[[nodiscard]] const char *db_network_get_name(const struct db_network *n);

// get_nick, get_username, get_realname (net may be nullptr; borrowed)
[[nodiscard]] const char *db_get_nick(const struct db_user *u, const struct db_network *n);
[[nodiscard]] const char *db_get_username(const struct db_user *u, const struct db_network *n);
[[nodiscard]] const char *db_get_realname(const struct db_user *u, const struct db_network *n);

enum message_filter {
	// TODO (upstream): use customizable user defaults for FILTER_DEFAULT
	FILTER_DEFAULT = 0,
	FILTER_NONE = 1,
	FILTER_HIGHLIGHT = 2,
	FILTER_MESSAGE = 3,
};

[[nodiscard]] enum message_filter message_filter_from_i64(int64_t v);

struct db_channel {
	int64_t id;
	char *name;
	char *key;
	bool detached;
	char *detached_internal_msg_id;
	enum message_filter relay_detached;
	enum message_filter reattach_on;
	uint64_t detach_after_ns;
	enum message_filter detach_on;
};

void db_channel_init(struct db_channel *c);
void db_channel_free(struct db_channel *c);

struct db_device_certificate {
	int64_t id;
	char *label;
	struct strbuf fingerprint; // SHA-512 hash
	struct db_time last_used;
	char *last_ip;
};

void db_device_certificate_init(struct db_device_certificate *c);
void db_device_certificate_free(struct db_device_certificate *c);
// DeviceCertificate::mark_used: now, and the canonical IP (or "")
void db_device_certificate_mark_used(struct db_device_certificate *c, const char *ip);

struct db_delivery_receipt {
	int64_t id;
	char *target; // channel or nick
	char *client;
	char *internal_msg_id;
};

void db_delivery_receipt_init(struct db_delivery_receipt *r);
void db_delivery_receipt_free(struct db_delivery_receipt *r);

struct db_read_receipt {
	int64_t id;
	char *target; // channel or nick
	struct db_time timestamp;
};

void db_read_receipt_init(struct db_read_receipt *r);
void db_read_receipt_free(struct db_read_receipt *r);

struct db_webpush_config {
	int64_t id;
	char *vapid_public;
	char *vapid_private;
};

void db_webpush_config_init(struct db_webpush_config *c);
void db_webpush_config_free(struct db_webpush_config *c);

struct db_webpush_subscription {
	int64_t id;
	char *endpoint;
	struct db_time created_at; // read-only
	struct db_time updated_at; // read-only
	char *key_auth;
	char *key_p256dh;
	char *key_vapid;
};

void db_webpush_subscription_init(struct db_webpush_subscription *s);
void db_webpush_subscription_free(struct db_webpush_subscription *s);

struct db_message_target {
	int64_t id;
	char *target;
	bool pinned, muted, blocked;
};

void db_message_target_free(struct db_message_target *t);

struct db_target_last {
	char *name;
	struct db_time latest_message;
};

struct db_message_options {
	int64_t after_id;
	struct db_time after_time;
	struct db_time before_time;
	size_t limit;
	bool events;
	const char *sender; // "" or nullptr: any
	const char *text; // "" or nullptr: no search
	bool take_last;
};

struct db_message_row {
	int64_t id;
	char *time;
	struct irc_msg *message;
};

struct db_stats {
	int64_t users, networks, channels;
};

typedef VEC(struct db_user) db_user_vec;
typedef VEC(struct db_network) db_network_vec;
typedef VEC(struct db_channel) db_channel_vec;
typedef VEC(struct db_device_certificate) db_cert_vec;
typedef VEC(struct db_delivery_receipt) db_receipt_vec;
typedef VEC(struct db_webpush_config) db_webpush_config_vec;
typedef VEC(struct db_webpush_subscription) db_webpush_sub_vec;
typedef VEC(struct db_message_target) db_message_target_vec;
typedef VEC(struct db_target_last) db_target_last_vec;
typedef VEC(struct db_message_row) db_message_row_vec;
typedef VEC(int64_t) i64vec;

void db_user_vec_free(db_user_vec *v);
void db_network_vec_free(db_network_vec *v);
void db_channel_vec_free(db_channel_vec *v);
void db_cert_vec_free(db_cert_vec *v);
void db_receipt_vec_free(db_receipt_vec *v);
void db_webpush_config_vec_free(db_webpush_config_vec *v);
void db_webpush_sub_vec_free(db_webpush_sub_vec *v);
void db_message_target_vec_free(db_message_target_vec *v);
void db_target_last_vec_free(db_target_last_vec *v);
void db_message_row_vec_free(db_message_row_vec *v);

struct db;

// The current schema version, including local migrations
[[nodiscard]] int64_t db_schema_version(void);
// Migration i's SQL (1 <= i < db_schema_version()), for tests
[[nodiscard]] const char *db_migration(size_t i);

[[nodiscard]] bool db_open(const char *source, struct db **out, struct err **err);
[[nodiscard]] bool db_open_in_memory(struct db **out, struct err **err);
// Opens an existing SQLite handle (takes ownership, closed on failure):
// migrations of upstream databases are tested this way
struct sqlite3;
[[nodiscard]] bool db_open_handle(struct sqlite3 *conn, struct db **out, struct err **err);
// Closes the connection; every statement must be finalized
void db_close(struct db *db);
// The underlying handle, for tests (borrowed)
[[nodiscard]] struct sqlite3 *db_handle(struct db *db);
// Runs the migrations again (a no-op on an up-to-date database)
[[nodiscard]] bool db_upgrade(struct db *db, struct err **err);

[[nodiscard]] bool db_stats(struct db *db, struct db_stats *out, struct err **err);
// The fraction of the database file made of free pages
[[nodiscard]] bool db_free_page_ratio(struct db *db, double *out, struct err **err);
// Merges the search index after deleting messages, and truncates the WAL
[[nodiscard]] bool db_optimize(struct db *db, struct err **err);
// Frees the page cache
[[nodiscard]] bool db_shrink_memory(struct db *db, struct err **err);
// Reclaims the free pages, by rewriting the whole database file
[[nodiscard]] bool db_vacuum(struct db *db, struct err **err);

// Users
[[nodiscard]] bool db_list_users(struct db *db, db_user_vec *out, struct err **err);
// *found is false if the user doesn't exist
[[nodiscard]] bool db_get_user(struct db *db, const char *username, struct db_user *out,
			       bool *found, struct err **err);
// *out is nullptr if there's no such user
[[nodiscard]] bool db_get_username_by_id(struct db *db, int64_t id, char **out, struct err **err);
// The enabled users who haven't interacted since limit
[[nodiscard]] bool db_list_inactive_usernames(struct db *db, struct timestamp limit, strvec *out,
					      struct err **err);
// Updates an existing user, except for its password
[[nodiscard]] bool db_store_user_keep_password(struct db *db, const struct db_user *u,
					       struct err **err);
// Replaces a password hash with a stronger hash of the same password,
// unless it was changed meanwhile
[[nodiscard]] bool db_upgrade_user_password(struct db *db, int64_t id, const char *old,
					    const char *new_hash, struct err **err);
[[nodiscard]] bool db_set_user_downstream_interacted_at(struct db *db, int64_t id,
							struct timestamp t, struct err **err);
// Inserts (id 0, sets u->id) or updates a user
[[nodiscard]] bool db_store_user(struct db *db, struct db_user *u, struct err **err);
[[nodiscard]] bool db_delete_user(struct db *db, int64_t id, struct err **err);

// Device certificates
[[nodiscard]] bool db_get_device_certificate(struct db *db, struct str fingerprint, int64_t *user,
					     struct db_device_certificate *out, bool *found,
					     struct err **err);
[[nodiscard]] bool db_list_device_certificates(struct db *db, int64_t user_id, db_cert_vec *out,
					       struct err **err);
// Fails with DB_ERR_DUPLICATE_DEVICE_CERTIFICATE if the fingerprint is
// already registered
[[nodiscard]] bool db_store_device_certificate(struct db *db, int64_t user_id,
					       struct db_device_certificate *c, struct err **err);
[[nodiscard]] bool db_delete_device_certificate(struct db *db, int64_t user_id,
						struct str fingerprint, struct err **err);

// Networks and channels
[[nodiscard]] bool db_list_networks(struct db *db, int64_t user_id, db_network_vec *out,
				    struct err **err);
[[nodiscard]] bool db_store_network(struct db *db, int64_t user_id, struct db_network *n,
				    struct err **err);
[[nodiscard]] bool db_delete_network(struct db *db, int64_t id, struct err **err);
[[nodiscard]] bool db_list_channels(struct db *db, int64_t network_id, db_channel_vec *out,
				    struct err **err);
[[nodiscard]] bool db_store_channel(struct db *db, int64_t network_id, struct db_channel *c,
				    struct err **err);
[[nodiscard]] bool db_delete_channel(struct db *db, int64_t id, struct err **err);

// Receipts
[[nodiscard]] bool db_list_delivery_receipts(struct db *db, int64_t network_id, db_receipt_vec *out,
					     struct err **err);
// Replaces the receipts of a client ("" for none), writing only what
// changed; sets the IDs of the receipts
[[nodiscard]] bool db_store_client_delivery_receipts(struct db *db, int64_t network_id,
						     const char *client,
						     struct db_delivery_receipt *receipts, size_t n,
						     struct err **err);
[[nodiscard]] bool db_get_read_receipt(struct db *db, int64_t network_id, const char *name,
				       struct db_read_receipt *out, bool *found, struct err **err);
[[nodiscard]] bool db_store_read_receipt(struct db *db, int64_t network_id,
					 struct db_read_receipt *r, struct err **err);

// Web Push
[[nodiscard]] bool db_list_webpush_configs(struct db *db, db_webpush_config_vec *out,
					   struct err **err);
[[nodiscard]] bool db_store_webpush_config(struct db *db, struct db_webpush_config *c,
					   struct err **err);
// The networks a user has subscriptions for (0 for the bouncer connection)
[[nodiscard]] bool db_list_webpush_networks(struct db *db, int64_t user_id, i64vec *out,
					    struct err **err);
[[nodiscard]] bool db_list_webpush_subscriptions(struct db *db, int64_t user_id, int64_t network_id,
						 db_webpush_sub_vec *out, struct err **err);
[[nodiscard]] bool db_store_webpush_subscription(struct db *db, int64_t user_id, int64_t network_id,
						 struct db_webpush_subscription *s,
						 struct err **err);
[[nodiscard]] bool db_delete_webpush_subscription(struct db *db, int64_t id, struct err **err);

// Messages
// The ID of the last text message stored for a target, or 0
[[nodiscard]] bool db_get_message_last_id(struct db *db, int64_t network_id, const char *name,
					  int64_t *out, struct err **err);
// A target with a zero ID if it doesn't exist
[[nodiscard]] bool db_get_message_target(struct db *db, int64_t network_id, const char *target,
					 struct db_message_target *out, struct err **err);
[[nodiscard]] bool db_list_message_targets(struct db *db, int64_t network_id,
					   db_message_target_vec *out, struct err **err);
[[nodiscard]] bool db_store_message_target(struct db *db, int64_t network_id,
					   struct db_message_target *t, struct err **err);
// Stores messages of a target in a transaction; appends their IDs to ids
[[nodiscard]] bool db_store_messages(struct db *db, int64_t network_id, const char *name,
				     struct irc_msg *const *msgs, size_t n, i64vec *ids,
				     struct err **err);
[[nodiscard]] bool db_list_message_last_per_target(struct db *db, int64_t network_id,
						   const struct db_message_options *o,
						   db_target_last_vec *out, struct err **err);
[[nodiscard]] bool db_list_messages(struct db *db, int64_t network_id, const char *name,
				    const struct db_message_options *o, irc_msg_vec *out,
				    struct err **err);
[[nodiscard]] bool db_list_message_rows(struct db *db, int64_t network_id, const char *name,
					const struct db_message_options *o, db_message_row_vec *out,
					struct err **err);
// Deletes up to limit messages older than before; *deleted is less than
// limit once there are none left
[[nodiscard]] bool db_delete_messages_before(struct db *db, struct timestamp before, size_t limit,
					     size_t *deleted, struct err **err);
// Quotes each token of a search query so FTS5 operators match literally
void quote_fts_query(struct strbuf *out, struct str query);
