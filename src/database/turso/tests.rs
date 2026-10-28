//! Exercise the real HTTP transport against a local Platform/Hrana server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rusqlite::types::ValueRef;
use serde_json::Value as Json;

use super::*;
use crate::database::{DeliveryReceipt, MessageOptions, Network, ReadReceipt};
use crate::msg;

struct State {
    root: PathBuf,
    databases: HashMap<String, String>,
    streams: HashMap<String, (String, rusqlite::Connection)>,
    next_stream: u64,
    fail_create: bool,
    fail_delete: bool,
    fail_sql: Option<String>,
    bad_group: bool,
}

struct Cloud {
    url: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Cloud {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("soju-turso-{}", database_name()));
        std::fs::create_dir(&root).unwrap();
        let state = Arc::new(Mutex::new(State {
            root,
            databases: HashMap::new(),
            streams: HashMap::new(),
            next_stream: 0,
            fail_create: false,
            fail_delete: false,
            fail_sql: None,
            bad_group: false,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &mut thread_state.lock().unwrap()),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("mock accept: {e}"),
                }
            }
        });
        Self {
            url,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn config(&self) -> config::Turso {
        config::Turso {
            organization: "example".into(),
            group: "soju".into(),
            controller: "soju-controller".into(),
            api_token: "platform-secret".into(),
            group_token: "sql-secret".into(),
        }
    }

    fn open(&self) -> Result<Turso> {
        Turso::open_at(
            &self.config(),
            format!("{}/v1", self.url),
            Some(self.url.clone()),
        )
    }

    fn database(&self) -> Result<crate::database::Database> {
        let cloud = Box::new(self.open()?);
        Ok(crate::database::Database::from_cloud(cloud))
    }
}

impl Drop for Cloud {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        let mut state = self.state.lock().unwrap();
        state.streams.clear();
        std::fs::remove_dir_all(&state.root).unwrap();
    }
}

fn serve(mut stream: TcpStream, state: &mut State) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let parts: Vec<_> = first.split_whitespace().collect();
    if parts.len() != 3 {
        return;
    }
    let (method, path) = (parts[0], parts[1]);
    let mut length = 0;
    let mut auth = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
            if name.eq_ignore_ascii_case("authorization") {
                auth = value.trim().to_owned();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body: Json = if body.is_empty() {
        Json::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let expected_auth = if path.starts_with("/v1/") {
        "Bearer platform-secret"
    } else {
        "Bearer sql-secret"
    };
    let (status, response) = if auth != expected_auth {
        (401, Json::Null)
    } else if path.starts_with("/v1/") {
        let collection = "/v1/organizations/example/databases";
        if path == collection && method == "POST" {
            if state.fail_create {
                (500, Json::Null)
            } else {
                let name = body["name"].as_str().unwrap().to_owned();
                let group = body["group"].as_str().unwrap().to_owned();
                if state.databases.contains_key(&name) {
                    (409, Json::Null)
                } else {
                    state.databases.insert(name.clone(), group);
                    (200, json!({"database":{"Name":name}}))
                }
            }
        } else {
            let name = path.strip_prefix(&format!("{collection}/")).unwrap();
            if method == "DELETE" {
                if state.fail_delete {
                    (503, Json::Null)
                } else if state.databases.remove(name).is_some() {
                    state.streams.retain(|_, (db, _)| db != name);
                    let _ = std::fs::remove_file(state.root.join(name));
                    (204, Json::Null)
                } else {
                    (404, Json::Null)
                }
            } else if let Some(group) = state.databases.get(name) {
                (
                    200,
                    json!({"database":{"Name":name, "Hostname":format!("{name}-example.turso.io"),
                    "group":if state.bad_group { "different-group" } else { group }}}),
                )
            } else {
                (404, Json::Null)
            }
        }
    } else {
        let name = path
            .strip_prefix("/db/")
            .unwrap()
            .strip_suffix("/v2/pipeline")
            .unwrap();
        if state.fail_sql.as_deref() == Some(name) {
            (503, Json::Null)
        } else {
            (200, pipeline(state, name, &body))
        }
    };
    let bytes = if status == 204 {
        Vec::new()
    } else {
        serde_json::to_vec(&response).unwrap()
    };
    write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
    stream.write_all(&bytes).unwrap();
}

fn pipeline(state: &mut State, name: &str, body: &Json) -> Json {
    assert!(state.databases.contains_key(name));
    let mut conn = if let Some(baton) = body["baton"].as_str() {
        let (database, conn) = state.streams.remove(baton).expect("valid single-use baton");
        assert_eq!(database, name);
        conn
    } else {
        rusqlite::Connection::open(state.root.join(name)).unwrap()
    };
    let mut closed = false;
    let results: Vec<_> = body["requests"].as_array().unwrap().iter().map(|request| {
        let result = match request["type"].as_str().unwrap() {
            "execute" => execute(&mut conn, &request["stmt"]).map(|r| json!({"type":"execute", "result":r})),
            "sequence" => conn.execute_batch(request["sql"].as_str().unwrap()).map(|_| json!({"type":"sequence"})),
            "close" => { assert!(!closed); closed = true; Ok(json!({"type":"close"})) },
            other => panic!("unsupported mock operation {other}"),
        };
        match result {
            Ok(response) => json!({"type":"ok", "response":response}),
            Err(e) => json!({"type":"error", "error":{"code": match e {
                rusqlite::Error::SqliteFailure(err, _) if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE",
                _ => "SQLITE_ERROR",
            }, "message":e.to_string()}}),
        }
    }).collect();
    let baton = if closed {
        None
    } else {
        state.next_stream += 1;
        let baton = state.next_stream.to_string();
        state.streams.insert(baton.clone(), (name.into(), conn));
        Some(baton)
    };
    json!({"baton":baton, "base_url":null, "results":results})
}

fn execute(conn: &mut rusqlite::Connection, stmt: &Json) -> rusqlite::Result<Json> {
    let mut query = conn.prepare(stmt["sql"].as_str().unwrap())?;
    if let Some(args) = stmt["args"].as_array() {
        for (i, arg) in args.iter().enumerate() {
            query.raw_bind_parameter(i + 1, super::super::connection::decode(arg)?)?;
        }
    }
    if let Some(args) = stmt["named_args"].as_array() {
        for arg in args {
            let index = query
                .parameter_index(&format!(":{}", arg["name"].as_str().unwrap()))?
                .unwrap();
            query.raw_bind_parameter(index, super::super::connection::decode(&arg["value"])?)?;
        }
    }
    let cols: Vec<_> = query
        .column_names()
        .iter()
        .map(|name| json!({"name":name, "decltype":null}))
        .collect();
    let count = cols.len();
    let mut result_rows = Vec::new();
    if count == 0 {
        query.raw_execute()?;
    } else {
        let mut rows = query.raw_query();
        while let Some(row) = rows.next()? {
            let values: Vec<_> = (0..count)
                .map(|i| match row.get_ref(i).unwrap() {
                    ValueRef::Null => json!({"type":"null"}),
                    ValueRef::Integer(n) => json!({"type":"integer", "value":n.to_string()}),
                    ValueRef::Real(n) => json!({"type":"float", "value":n}),
                    ValueRef::Text(s) => json!({"type":"text", "value":String::from_utf8_lossy(s)}),
                    ValueRef::Blob(b) => json!({"type":"blob", "base64":STANDARD.encode(b)}),
                })
                .collect();
            result_rows.push(values);
        }
    }
    Ok(
        json!({"cols":cols, "rows":result_rows, "affected_row_count":conn.changes(),
        "last_insert_rowid":conn.last_insert_rowid().to_string()}),
    )
}

fn user(db: &crate::database::Database, name: &str) -> User {
    let mut user = User::new(name);
    user.set_password("secret").unwrap();
    user.admin = name == "alice";
    db.store_user(&mut user).unwrap();
    user
}

#[tokio::test(flavor = "current_thread")]
async fn provisioning_authentication_isolation_and_restart() {
    let cloud = Cloud::new();
    let db = Arc::new(cloud.database().unwrap());
    let alice = user(&db, "alice");
    let bob = user(&db, "bob");
    crate::auth::PlainAuthenticator::Internal
        .auth_plain(&db, "alice", "secret", "")
        .await
        .unwrap();
    assert!(
        crate::auth::PlainAuthenticator::Internal
            .auth_plain(&db, "alice", "incorrect", "")
            .await
            .is_err()
    );
    let mut auth = db.get_user("alice").unwrap().unwrap();
    assert!(auth.admin);
    auth.check_password("secret").unwrap();
    assert!(auth.check_password("wrong").is_err());
    let mut a_net = Network::new("ircs://irc.example.org");
    let mut b_net = Network::new("ircs://irc.example.org");
    db.store_network(alice.id, &mut a_net).unwrap();
    db.store_network(bob.id, &mut b_net).unwrap();
    assert_ne!(a_net.id, b_net.id);
    let messages = vec![msg!("PRIVMSG", "#a", "alice's searchable message")];
    let ids = db.store_messages(a_net.id, "#a", &messages).unwrap();
    db.store_messages(b_net.id, "#a", &[msg!("PRIVMSG", "#a", "bob private")])
        .unwrap();
    let mut receipts = vec![DeliveryReceipt {
        target: "#a".into(),
        internal_msg_id: "cursor".into(),
        ..Default::default()
    }];
    db.store_client_delivery_receipts(a_net.id, "phone", &mut receipts)
        .unwrap();
    let mut read = ReadReceipt {
        target: "#a".into(),
        timestamp: Some(chrono::Utc::now()),
        ..Default::default()
    };
    db.store_read_receipt(a_net.id, &mut read).unwrap();
    let mut channel = crate::database::Channel {
        name: "#a".into(),
        ..Default::default()
    };
    db.store_channel(a_net.id, &mut channel).unwrap();
    let mut cert = crate::database::DeviceCertificate {
        label: "phone".into(),
        fingerprint: vec![1, 2, 3],
        last_used: Some(chrono::Utc::now()),
        ..Default::default()
    };
    db.store_device_certificate(alice.id, &mut cert).unwrap();
    assert_eq!(
        db.get_device_certificate(&cert.fingerprint)
            .unwrap()
            .unwrap()
            .0,
        alice.id
    );
    let mut duplicate = cert.clone();
    duplicate.id = 0;
    assert!(
        db.store_device_certificate(bob.id, &mut duplicate)
            .unwrap_err()
            .is::<crate::database::DuplicateDeviceCertificate>()
    );
    let names: Vec<String> = db
        .account_conn_for_test()
        .prepare("SELECT name FROM UserDatabase")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(names.len(), 2);
    for name in &names {
        assert_eq!(name.len(), 25);
        assert!(
            name.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }
    assert_ne!(names[0], names[1]);
    let controller_path = cloud.state.lock().unwrap().root.join("soju-controller");
    let controller = rusqlite::Connection::open(controller_path).unwrap();
    let tables: Vec<String> = controller
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        !tables
            .iter()
            .any(|t| ["Message", "Network", "Channel"].contains(&t.as_str()))
    );
    drop(db);
    let db = cloud.database().unwrap();
    let options = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(a_net.id, "#a", &options).unwrap(),
        messages
    );
    assert_eq!(
        db.list_messages(b_net.id, "#a", &options).unwrap()[0].trailing(),
        "bob private"
    );
    assert_eq!(db.get_message_last_id(a_net.id, "#a").unwrap(), ids[0]);
    assert_eq!(
        db.list_messages(
            a_net.id,
            "#a",
            &MessageOptions {
                text: "searchable".into(),
                ..options
            }
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        db.list_delivery_receipts(a_net.id).unwrap()[0].internal_msg_id,
        "cursor"
    );
    assert!(db.get_read_receipt(a_net.id, "#a").unwrap().is_some());
    assert_eq!(db.stats().unwrap().users, 2);
    assert_eq!(db.stats().unwrap().networks, 2);
    db.delete_channel(channel.id).unwrap();
    let before = chrono::Utc::now() + chrono::Duration::days(1);
    assert_eq!(db.delete_messages_before(before, 10).unwrap(), 2);
    let new_ids = db.store_messages(a_net.id, "#a", &messages).unwrap();
    assert!(new_ids[0] > ids[0]);
    db.optimize().unwrap();
    db.delete_network(a_net.id).unwrap();
    db.delete_user(alice.id).unwrap();
    assert!(db.get_user("alice").unwrap().is_none());
    assert_eq!(db.list_networks(bob.id).unwrap().len(), 1);
    assert_eq!(cloud.state.lock().unwrap().databases.len(), 2);
    drop(db);
    let db = cloud.database().unwrap();
    assert_eq!(db.list_users().unwrap().len(), 1);
}

#[test]
fn creation_and_deletion_resume_after_failure() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    cloud.state.lock().unwrap().fail_create = true;
    let mut alice = User::new("alice");
    alice.set_password("secret").unwrap();
    assert!(db.store_user(&mut alice).is_err());
    assert!(db.get_user("alice").unwrap().is_none());
    assert!(db.list_users().unwrap().is_empty());
    assert_eq!(cloud.state.lock().unwrap().databases.len(), 1);
    drop(db);
    cloud.state.lock().unwrap().fail_create = false;
    let db = cloud.database().unwrap();
    assert!(db.get_user("alice").unwrap().is_some());
    assert_eq!(cloud.state.lock().unwrap().databases.len(), 2);
    cloud.state.lock().unwrap().fail_delete = true;
    assert!(db.delete_user(alice.id).is_err());
    assert!(db.get_user("alice").unwrap().is_none());
    drop(db);
    cloud.state.lock().unwrap().fail_delete = false;
    let db = cloud.database().unwrap();
    assert!(db.list_users().unwrap().is_empty());
    assert_eq!(cloud.state.lock().unwrap().databases.len(), 1);
}

#[test]
fn wrong_group_and_sql_failure_leave_no_partial_message() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let alice = user(&db, "alice");
    let mut net = Network::new("ircs://irc.example.org");
    db.store_network(alice.id, &mut net).unwrap();
    let mut bad = msg!("PRIVMSG", "#a", "bad");
    bad.tags.set("time", "invalid time");
    assert!(
        db.store_messages(net.id, "#a", &[msg!("PRIVMSG", "#a", "good"), bad])
            .is_err()
    );
    assert_eq!(db.get_message_last_id(net.id, "#a").unwrap(), 0);
    let mut duplicate = User::new("alice");
    assert!(db.store_user(&mut duplicate).is_err());
    assert_eq!(cloud.state.lock().unwrap().databases.len(), 2);
    drop(db);
    cloud.state.lock().unwrap().bad_group = true;
    assert!(
        cloud
            .open()
            .err()
            .unwrap()
            .to_string()
            .contains("configured group")
    );
}

#[test]
fn names_retain_all_128_bits() {
    let mut names = std::collections::HashSet::new();
    for _ in 0..1000 {
        assert!(names.insert(database_name()));
    }
    assert!(
        names
            .iter()
            .all(|n| n.len() == 25 && u128::from_str_radix(n, 36).is_ok())
    );
}

#[test]
fn authorization_failures_do_not_expose_credentials() {
    let cloud = Cloud::new();
    let mut cfg = cloud.config();
    cfg.api_token = "wrong-api-secret".into();
    let error = Turso::open_at(&cfg, format!("{}/v1", cloud.url), Some(cloud.url.clone()))
        .err()
        .unwrap();
    assert!(error.to_string().contains("401"));
    assert!(!error.to_string().contains("wrong-api-secret"));
    cfg = cloud.config();
    cfg.group_token = "wrong-sql-secret".into();
    let error = Turso::open_at(&cfg, format!("{}/v1", cloud.url), Some(cloud.url.clone()))
        .err()
        .unwrap();
    assert!(error.to_string().contains("authorization failed"));
    assert!(!error.to_string().contains("wrong-sql-secret"));
}

#[test]
fn user_schema_migration_preserves_messages() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let alice = user(&db, "alice");
    let mut net = Network::new("ircs://irc.example.org");
    db.store_network(alice.id, &mut net).unwrap();
    let messages = vec![msg!("PRIVMSG", "#a", "old message")];
    db.store_messages(net.id, "#a", &messages).unwrap();
    let name: String = db
        .account_conn_for_test()
        .query_row(
            "SELECT name FROM UserDatabase WHERE user = ?",
            [alice.id],
            |r| r.get(0),
        )
        .unwrap();
    drop(db);
    {
        let path = cloud.state.lock().unwrap().root.join(name);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("DROP INDEX MessageTextIndex; PRAGMA user_version = 30;")
            .unwrap();
    }
    let db = cloud.database().unwrap();
    assert_eq!(
        db.list_messages(
            net.id,
            "#a",
            &MessageOptions {
                limit: 10,
                ..Default::default()
            }
        )
        .unwrap(),
        messages
    );
}
