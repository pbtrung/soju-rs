# Turso Cloud storage

`message-store turso` stores bouncer data in Turso Cloud. It creates a
controller database and one database per user in an existing Turso group.
It replaces the local `db` backend for the whole bouncer; omit the `db`
directive when using it.

The controller is the account directory: usernames, bcrypt password hashes,
account settings and permissions, authentication certificates, and the link
and provisioning state for each user database. It also keeps the bouncer's
Web Push signing identity. Networks, upstream credentials, channels, delivery
and read receipts, Web Push subscriptions, messages, and the full-text search
index live in the user's database. User databases contain a password-free
account row as a foreign-key anchor. No local database file is needed.

## Configuration

Create a Turso organization and group first, then configure:

```scfg
listen irc+insecure://localhost:6667
listen unix+admin://
message-store turso
turso-organization my-organization
turso-group bouncer
turso-controller soju-controller
turso-group-token "GROUP_SQL_TOKEN"
turso-api-token "PLATFORM_API_TOKEN"
```

`turso-controller` is optional and defaults to `soju-controller`. Use a
different controller name and a separate group for each independent bouncer.
Run one soju instance per controller. Storage and credential configuration
changes require a restart. Changing `turso-controller` selects a different
account directory; it does not rename or migrate the existing databases.

There are two different kinds of Turso credential:

- `turso-group-token`: a full-access SQL token for every database in the
  configured group. For example, create one with
  `turso group tokens create bouncer --expiration never`.
- `turso-api-token`: a Platform API token with `read`, `db:create`, and
  `db:delete` permissions for the organization/group. Provisioning uses it to
  retrieve, create and delete databases. A group SQL token cannot perform
  these Platform API operations. See [Turso API authentication].

Keep the configuration readable only by the account running soju; it contains
both tokens. Tokens must remain valid for the bouncer to operate. Rotate them
in the configuration and restart soju. SQL uses HTTPS and Turso's
[SQL-over-HTTP protocol]. The client tracks replication indexes so subsequent
reads wait for its previous writes. Remote event handlers run on blocking
workers and independent HTTP streams run concurrently, including when soju
uses a single-threaded runtime. Cloud round-trip latency still affects the
user issuing a command; place soju near the group's primary region.

Writes are not automatically replayed after network errors. If the server
commits a write but its response is lost, the operation reports failure and
its outcome is uncertain. Check the saved state before repeating it. A failed
transaction is discarded instead of continuing on a new auto-commit stream.

## Create an administrator

Before starting soju, bootstrap an administrator and their database:

```sh
sojuctl -config /etc/soju/config turso create-admin -username alice -password secret
soju -config /etc/soju/config
```

This explicit bootstrap command connects directly to Turso and needs no
running server or admin socket. Run it while soju is stopped, or restart soju
afterwards to load the new user. It creates a new account and fails if the
username already exists.

With soju running, the existing admin-socket commands work too:

```sh
sojuctl -config /etc/soju/config user create -username bob -password secret -admin
```

Both paths provision the user's database before enabling sign-in. Clients
sign in with their bouncer username and password as usual.

## Provisioning and recovery

Startup creates the controller if it is missing, opens every linked user
database, and initializes or upgrades their schemas before accepting clients.
Each user database name is a fixed-width, 25-character base36 encoding of
128 cryptographically random bits, using only `a-z0-9`. Names are unrelated
to usernames and are retained across restarts.

Account creation first commits the account and database name to the
controller with a `provisioning` state. If database creation or schema setup
fails, sign-in stays disabled for that account. Fix the credentials or cloud
availability issue and restart soju to resume with the same database name.
Startup fails if any linked database cannot be initialized. Existing database
names must belong to the configured group. Once a database is marked `ready`,
startup refuses to recreate it if it disappears; restore the database and
restart. Only new accounts in `provisioning` state can create a missing user
database. Each user database also records its controller and owner to reject
links to another account's data.

Back up the controller together with the user databases. The controller's
links and password hashes are needed to recover the account directory; user
database names cannot reconstruct it. If the controller itself is missing,
startup creates an empty directory, as it does on the first run.

Deleting a user through BouncerServ deletes their cloud database too. The
controller records `deleting` first; interrupted deletions resume on startup
without recreating the database. Turso delete protection must be disabled for
user databases that should be removable through soju.
Deletion validates the link and group again before calling the Platform API;
it refuses to delete the controller or a database moved to another group.

History, search, message expiry and receipt behavior match the `db` store.
Expiry removes messages and optimizes each user's FTS index. It never sends
`VACUUM`, `PRAGMA wal_checkpoint`, `PRAGMA shrink_memory` or file-space probes
to Turso, including after the startup expiry pass. These operations are for
local SQLite; the hosted SQL interface rejects client-issued `VACUUM` and
WAL checkpoints. Turso migrations also omit the local-only `ANALYZE` step.
See the [server SQL allowlist] for these restrictions. Existing local SQLite
data is not imported automatically when switching to Turso.

The pre-release [Turso audit](turso-audit.md) records the fault cases covered
by regression tests and the remaining verification limits.

[Turso API authentication]: https://docs.turso.tech/api-reference/authentication
[SQL-over-HTTP protocol]: https://docs.turso.tech/sdk/http/reference
[server SQL allowlist]: https://github.com/tursodatabase/libsql/blob/main/libsql-server/src/query_analysis.rs
