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

CREATE TABLE DeviceCertificate (
	id INTEGER PRIMARY KEY,
	user INTEGER NOT NULL,
	label TEXT NOT NULL,
	fingerprint BLOB NOT NULL UNIQUE,
	last_used TEXT NOT NULL,
	last_ip TEXT NOT NULL,
	FOREIGN KEY(user) REFERENCES User(id)
);

CREATE TABLE WebPushConfig (
	id INTEGER PRIMARY KEY,
	created_at TEXT NOT NULL,
	vapid_key_public TEXT NOT NULL,
	vapid_key_private TEXT NOT NULL,
	UNIQUE(vapid_key_public)
);

CREATE TABLE UserDatabase (
    user INTEGER PRIMARY KEY REFERENCES User(id),
    name TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('provisioning', 'ready', 'deleting'))
);
