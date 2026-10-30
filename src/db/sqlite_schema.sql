CREATE TABLE User (
	id INTEGER PRIMARY KEY,
	username TEXT NOT NULL UNIQUE,
	password TEXT,
	admin INTEGER NOT NULL DEFAULT 0,
	realname TEXT,
	nick TEXT,
	created_at TEXT NOT NULL,
	enabled INTEGER NOT NULL DEFAULT 1,
	downstream_interacted_at TEXT,
	max_networks INTEGER NOT NULL DEFAULT -1
);

CREATE TABLE Network (
	id INTEGER PRIMARY KEY,
	name TEXT,
	user INTEGER NOT NULL,
	addr TEXT NOT NULL,
	nick TEXT,
	username TEXT,
	realname TEXT,
	certfp TEXT,
	pass TEXT,
	connect_commands TEXT,
	sasl_mechanism TEXT,
	sasl_plain_username TEXT,
	sasl_plain_password TEXT,
	sasl_external_cert BLOB,
	sasl_external_key BLOB,
	auto_away INTEGER NOT NULL DEFAULT 1,
	enabled INTEGER NOT NULL DEFAULT 1,
	FOREIGN KEY(user) REFERENCES User(id),
	UNIQUE(user, addr, nick),
	UNIQUE(user, name)
);

CREATE TABLE Channel (
	id INTEGER PRIMARY KEY,
	network INTEGER NOT NULL,
	name TEXT NOT NULL,
	key TEXT,
	detached INTEGER NOT NULL DEFAULT 0,
	detached_internal_msgid TEXT,
	relay_detached INTEGER NOT NULL DEFAULT 0,
	reattach_on INTEGER NOT NULL DEFAULT 0,
	detach_after INTEGER NOT NULL DEFAULT 0,
	detach_on INTEGER NOT NULL DEFAULT 0,
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, name)
);

CREATE TABLE DeviceCertificate (
	id INTEGER PRIMARY KEY,
	user INTEGER NOT NULL,
	label TEXT NOT NULL,
	fingerprint BLOB NOT NULL UNIQUE,
	last_used TEXT NOT NULL,
	last_ip TEXT NOT NULL,
	FOREIGN KEY(user) REFERENCES User(id)
);

CREATE INDEX DeviceCertificate_user_index ON DeviceCertificate(user);

CREATE TABLE DeliveryReceipt (
	id INTEGER PRIMARY KEY,
	network INTEGER NOT NULL,
	target TEXT NOT NULL,
	client TEXT,
	internal_msgid TEXT NOT NULL,
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, target, client)
);

CREATE INDEX DeliveryReceipt_client_index ON DeliveryReceipt(network, client);

CREATE TABLE ReadReceipt (
	id INTEGER PRIMARY KEY,
	network INTEGER NOT NULL,
	target TEXT NOT NULL,
	timestamp TEXT NOT NULL,
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, target)
);

CREATE TABLE WebPushConfig (
	id INTEGER PRIMARY KEY,
	created_at TEXT NOT NULL,
	vapid_key_public TEXT NOT NULL,
	vapid_key_private TEXT NOT NULL,
	UNIQUE(vapid_key_public)
);

CREATE TABLE WebPushSubscription (
	id INTEGER PRIMARY KEY,
	created_at TEXT NOT NULL,
	updated_at TEXT NOT NULL,
	user INTEGER NOT NULL,
	network INTEGER,
	endpoint TEXT NOT NULL,
	key_vapid TEXT,
	key_auth TEXT,
	key_p256dh TEXT,
	FOREIGN KEY(user) REFERENCES User(id),
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, endpoint)
);

CREATE INDEX WebPushSubscription_user_network_index ON WebPushSubscription(user, network);

-- Clustered by history order, like Turso user databases: a target's
-- recent messages share pages, and an event costs one row write. Text rows
-- also have an ID index for search results and delivery cursors, and a
-- time index for text-only history, which would otherwise read every
-- interleaved event.
CREATE TABLE Message (
	target INTEGER NOT NULL,
	time TEXT NOT NULL,
	id INTEGER NOT NULL,
	raw TEXT NOT NULL,
	sender TEXT NOT NULL,
	text TEXT,
	PRIMARY KEY(target, time, id),
	FOREIGN KEY(target) REFERENCES MessageTarget(id)
) WITHOUT ROWID;

CREATE UNIQUE INDEX MessageTextIdIndex ON Message(target, id) WHERE text IS NOT NULL;
CREATE INDEX MessageTextIndex ON Message(target, time) WHERE text IS NOT NULL;

CREATE TABLE MessageIdHighWater (
	id INTEGER PRIMARY KEY CHECK (id = 0),
	last_id INTEGER NOT NULL
);

CREATE TABLE MessageTarget (
	id INTEGER PRIMARY KEY,
	network INTEGER NOT NULL,
	target TEXT NOT NULL,
	pinned INTEGER NOT NULL DEFAULT 0,
	muted INTEGER NOT NULL DEFAULT 0,
	blocked INTEGER NOT NULL DEFAULT 0,
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, target)
);

CREATE INDEX MessageTarget_flags_index ON MessageTarget(network) WHERE pinned OR muted OR blocked;

-- The time of the latest message mentioning the user in each conversation,
-- see doc/unread.md. Keyed by name: unlike MessageTarget, a row never needs
-- an ID before the conversation's messages are stored.
CREATE TABLE TargetHighlight (
	network INTEGER NOT NULL,
	target TEXT NOT NULL,
	time TEXT NOT NULL,
	PRIMARY KEY(network, target),
	FOREIGN KEY(network) REFERENCES Network(id)
) WITHOUT ROWID;
