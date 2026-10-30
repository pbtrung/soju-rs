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
	FOREIGN KEY(user) REFERENCES TursoOwner(user),
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
	FOREIGN KEY(user) REFERENCES TursoOwner(user),
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, endpoint)
);

CREATE INDEX WebPushSubscription_user_network_index ON WebPushSubscription(user, network);

-- Clustered by history order: event playback, expiry and deletion seek the
-- primary key, and an event costs one row write. Text rows also have an ID
-- index for search results and insertion-order delivery cursors.
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
	highlight TEXT,
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, target)
);

CREATE INDEX MessageTarget_flags_index ON MessageTarget(network) WHERE pinned OR muted OR blocked;
