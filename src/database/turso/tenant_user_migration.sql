CREATE TABLE NetworkNew (
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
INSERT INTO NetworkNew SELECT * FROM Network;
DROP TABLE Network;
ALTER TABLE NetworkNew RENAME TO Network;
CREATE TABLE WebPushSubscriptionNew (
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
INSERT INTO WebPushSubscriptionNew SELECT * FROM WebPushSubscription;
DROP TABLE WebPushSubscription;
ALTER TABLE WebPushSubscriptionNew RENAME TO WebPushSubscription;
CREATE INDEX WebPushSubscription_user_network_index ON WebPushSubscription(user, network);
DROP TABLE DeviceCertificate;
DROP TABLE WebPushConfig;
DROP TABLE User;
