//! Exercise the real HTTP transport against a local Platform/Hrana server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use chrono::Utc;
use rusqlite::types::{Value, ValueRef};
use serde_json::Value as Json;

use super::*;
use crate::database::{DeliveryReceipt, MessageOptions, Network, ReadReceipt};
use crate::msg;

#[derive(Clone, Copy)]
enum Fault {
    LostStream,
    MissingResults,
    WrongResponseType,
    HttpAfter,
    EmptyBody,
}

struct State {
    root: PathBuf,
    databases: HashMap<String, String>,
    streams: HashMap<String, (String, rusqlite::Connection)>,
    next_stream: u64,
    fail_create: bool,
    fail_delete: bool,
    fail_sql: Option<String>,
    bad_group: bool,
    fault: Option<(String, Fault)>,
    delay: Option<(String, Duration)>,
    delay_before: Option<(String, Duration)>,
    max_body: Option<usize>,
    max_batch_lines: Option<usize>,
    request_sizes: Vec<usize>,
    requests: Vec<Json>,
    replication_index: u64,
    close_idle_streams: bool,
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
            fault: None,
            delay: None,
            delay_before: None,
            max_body: None,
            max_batch_lines: None,
            request_sizes: Vec::new(),
            requests: Vec::new(),
            replication_index: 0,
            close_idle_streams: true,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = thread_state.clone();
                        handlers.push(std::thread::spawn(move || serve(stream, &state)));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("mock accept: {e}"),
                }
            }
            for handler in handlers {
                handler.join().unwrap();
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

fn serve(mut stream: TcpStream, shared: &Arc<Mutex<State>>) {
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
    let mut state = shared.lock().unwrap();
    let contains_sql = |needle: &str| {
        body["requests"].as_array().is_some_and(|requests| {
            requests.iter().any(|r| {
                r["sql"]
                    .as_str()
                    .or_else(|| r["stmt"]["sql"].as_str())
                    .is_some_and(|sql| sql.contains(needle))
                    || r["batch"]["steps"].as_array().is_some_and(|steps| {
                        steps.iter().any(|s| {
                            s["stmt"]["sql"]
                                .as_str()
                                .is_some_and(|sql| sql.contains(needle))
                        })
                    })
            })
        })
    };
    let delay = state
        .delay
        .as_ref()
        .filter(|(sql, _)| contains_sql(sql))
        .map(|(_, delay)| *delay);
    let delay_before = state
        .delay_before
        .as_ref()
        .filter(|(sql, _)| contains_sql(sql))
        .map(|(_, delay)| *delay);
    if let Some(delay) = delay_before {
        state.delay_before = None;
        drop(state);
        std::thread::sleep(delay);
        state = shared.lock().unwrap();
    }
    let fault = if state
        .fault
        .as_ref()
        .is_some_and(|(sql, _)| contains_sql(sql))
    {
        state.fault.take().map(|(_, fault)| fault)
    } else {
        None
    };
    let expected_auth = if path.starts_with("/v1/") {
        "Bearer platform-secret"
    } else {
        "Bearer sql-secret"
    };
    let (mut status, mut response) = if auth != expected_auth {
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
        if body["baton"]
            .as_str()
            .is_some_and(|b| !state.streams.contains_key(b))
        {
            (400, Json::Null)
        } else if state.fail_sql.as_deref() == Some(name) {
            (503, Json::Null)
        } else {
            state.request_sizes.push(length);
            state.requests.push(body.clone());
            if state.max_body.is_some_and(|max| length > max) {
                (413, Json::Null)
            } else {
                (200, pipeline(&mut state, name, &body))
            }
        }
    };
    if let Some(fault) = fault {
        match fault {
            Fault::LostStream | Fault::HttpAfter | Fault::EmptyBody => {
                if let Some(baton) = response["baton"].as_str() {
                    state.streams.remove(baton);
                }
                response["baton"] = Json::Null;
                if matches!(fault, Fault::HttpAfter) {
                    status = 503;
                }
            }
            Fault::MissingResults => {
                response["results"].as_array_mut().unwrap().pop();
            }
            Fault::WrongResponseType => {
                response["results"][0]["response"]["type"] = "unexpected".into();
            }
        }
    }
    drop(state);
    if let Some(delay) = delay {
        std::thread::sleep(delay);
    }
    let bytes = if matches!(fault, Some(Fault::EmptyBody)) || status == 204 {
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
    state.replication_index += 1;
    let replication_index = state.replication_index.to_string();
    let results: Vec<_> = body["requests"].as_array().unwrap().iter().map(|request| {
        let result = match request["type"].as_str().unwrap() {
            "execute" => execute(&mut conn, &request["stmt"]).map(|r| json!({"type":"execute", "result":r})),
            "batch" => {
                let steps = request["batch"]["steps"].as_array().unwrap();
                let lines = steps.iter().filter(|s| s["stmt"]["sql"].as_str().unwrap().starts_with("INSERT INTO Message(")).count();
                if state.max_batch_lines.is_some_and(|max| lines > max) {
                    let mut errors = vec![Json::Null; steps.len()];
                    errors[0] = json!({"code":"TRANSACTION_TIMEOUT", "message":"mock processing budget"});
                    return json!({"type":"ok", "response":{"type":"batch", "result":{
                        "step_results":vec![Json::Null; steps.len()], "step_errors":errors,
                        "replication_index":replication_index}}});
                }
                let mut results: Vec<Json> = Vec::new();
                let mut errors = Vec::new();
                for step in steps {
                    let run = step["condition"].is_null() || match step["condition"]["type"].as_str().unwrap() {
                        "ok" => results[step["condition"]["step"].as_u64().unwrap() as usize].is_object(),
                        other => panic!("unsupported batch condition {other}"),
                    };
                    if !run { results.push(Json::Null); errors.push(Json::Null); continue; }
                    match execute(&mut conn, &step["stmt"]) {
                        Ok(result) => { results.push(result); errors.push(Json::Null); }
                        Err(e) => { results.push(Json::Null); errors.push(json!({"code":"SQLITE_ERROR", "message":e.to_string()})); }
                    }
                }
                Ok(json!({"type":"batch", "result":{"step_results":results, "step_errors":errors, "replication_index":replication_index}}))
            },
            "sequence" => validate_cloud_sql(request["sql"].as_str().unwrap())
                .and_then(|_| conn.execute_batch(request["sql"].as_str().unwrap())).map(|_| json!({"type":"sequence"})),
            "close" => { assert!(!closed); closed = true; Ok(json!({"type":"close"})) },
            other => panic!("unsupported mock operation {other}"),
        };
        match result {
            Ok(mut response) => {
                if response["type"] == "execute" { response["result"]["replication_index"] = replication_index.clone().into(); }
                json!({"type":"ok", "response":response})
            },
            Err(e) => json!({"type":"error", "error":{"code": match e {
                rusqlite::Error::SqliteFailure(err, _) if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE",
                _ => "SQLITE_ERROR",
            }, "message":e.to_string()}}),
        }
    }).collect();
    // Hosted Turso may close a stateless stream after COMMIT without an
    // explicit close request. Legacy libSQL keeps returning a baton instead.
    let baton = if closed || (state.close_idle_streams && conn.is_autocommit()) {
        None
    } else {
        state.next_stream += 1;
        let baton = state.next_stream.to_string();
        state.streams.insert(baton.clone(), (name.into(), conn));
        Some(baton)
    };
    json!({"baton":baton, "base_url":null, "results":results})
}

// Deliberately independent of the client's decoder: the mock must not hide
// encoding mistakes by sharing the same implementation on both ends.
fn decode_argument(arg: &Json) -> Value {
    match arg["type"].as_str().unwrap() {
        "null" => Value::Null,
        "integer" => Value::Integer(arg["value"].as_str().unwrap().parse().unwrap()),
        "float" => Value::Real(arg["value"].as_f64().unwrap()),
        "text" => Value::Text(arg["value"].as_str().unwrap().into()),
        "blob" => Value::Blob(
            STANDARD_NO_PAD
                .decode(arg["base64"].as_str().unwrap().trim_end_matches('='))
                .unwrap(),
        ),
        other => panic!("invalid protocol argument {other}"),
    }
}

fn execute(conn: &mut rusqlite::Connection, stmt: &Json) -> rusqlite::Result<Json> {
    validate_cloud_sql(stmt["sql"].as_str().unwrap())?;
    let mut query = conn.prepare(stmt["sql"].as_str().unwrap())?;
    if let Some(args) = stmt["args"].as_array() {
        for (i, arg) in args.iter().enumerate() {
            query.raw_bind_parameter(i + 1, decode_argument(arg))?;
        }
    }
    if let Some(args) = stmt["named_args"].as_array() {
        for arg in args {
            let name = arg["name"].as_str().unwrap();
            let name = if name.starts_with([':', '$', '@']) {
                name.to_owned()
            } else {
                format!(":{name}")
            };
            let index = query.parameter_index(&name)?.unwrap();
            query.raw_bind_parameter(index, decode_argument(&arg["value"]))?;
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
                    ValueRef::Blob(b) => json!({"type":"blob", "base64":STANDARD_NO_PAD.encode(b)}),
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

// SQLite accepts these statements, but the hosted SQL parser rejects them.
// Model those known restrictions so the local engine cannot mask regressions.
fn validate_cloud_sql(sql: &str) -> rusqlite::Result<()> {
    for statement in sql.to_ascii_lowercase().split(';').map(str::trim) {
        if [
            "vacuum",
            "analyze",
            "pragma wal_checkpoint",
            "pragma optimize",
            "pragma shrink_memory",
        ]
        .iter()
        .any(|prefix| statement.starts_with(prefix))
            || (statement.starts_with("pragma user_version")
                && (statement.contains('=') || statement.contains('(')))
        {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                std::io::Error::other("unsupported cloud SQL statement").into(),
            ));
        }
    }
    Ok(())
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
        conn.execute_batch(
            "DROP INDEX MessageTextIndex; DROP TABLE SojuSchemaVersion; PRAGMA user_version = 30;",
        )
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

fn linked_name(db: &crate::database::Database, user: i64) -> String {
    db.account_conn_for_test()
        .query_row(
            "SELECT name FROM UserDatabase WHERE user = ?",
            [user],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn missing_ready_database_is_not_recreated_empty() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let alice = user(&db, "alice");
    let name = linked_name(&db, alice.id);
    drop(db);
    cloud.state.lock().unwrap().databases.remove(&name);
    let error = cloud.open().err().unwrap();
    assert!(error.to_string().contains("refusing to recreate"));
    assert!(!cloud.state.lock().unwrap().databases.contains_key(&name));
}

#[test]
fn invalid_links_and_changed_groups_cannot_delete_other_databases() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let alice = user(&db, "alice");
    let name = linked_name(&db, alice.id);
    db.account_conn_for_test()
        .execute(
            "UPDATE UserDatabase SET name = 'soju-controller' WHERE user = ?",
            [alice.id],
        )
        .unwrap();
    assert!(
        db.delete_user(alice.id)
            .unwrap_err()
            .to_string()
            .contains("invalid Turso database link")
    );
    assert!(
        cloud
            .state
            .lock()
            .unwrap()
            .databases
            .contains_key("soju-controller")
    );
    db.account_conn_for_test()
        .execute(
            "UPDATE UserDatabase SET name = ? WHERE user = ?",
            params![name, alice.id],
        )
        .unwrap();
    cloud
        .state
        .lock()
        .unwrap()
        .databases
        .insert(name.clone(), "other-group".into());
    assert!(
        db.delete_user(alice.id)
            .unwrap_err()
            .to_string()
            .contains("outside the configured group")
    );
    assert!(cloud.state.lock().unwrap().databases.contains_key(&name));
    assert!(db.get_user("alice").unwrap().is_none());
}

#[test]
fn tenant_owner_mismatch_stops_startup() {
    for sql in [
        "UPDATE User SET username = 'different-owner'",
        "UPDATE TursoOwner SET controller = 'different-controller'",
    ] {
        let cloud = Cloud::new();
        let db = cloud.database().unwrap();
        let alice = user(&db, "alice");
        let name = linked_name(&db, alice.id);
        drop(db);
        rusqlite::Connection::open(cloud.state.lock().unwrap().root.join(&name))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        assert!(cloud.open().is_err());
        assert!(cloud.state.lock().unwrap().databases.contains_key(&name));
    }
}

#[test]
fn lost_or_malformed_stream_never_continues_a_transaction() {
    for fault in [
        Fault::LostStream,
        Fault::MissingResults,
        Fault::WrongResponseType,
        Fault::HttpAfter,
        Fault::EmptyBody,
    ] {
        let cloud = Cloud::new();
        let db = cloud.database().unwrap();
        let mut conn = db.account_conn_for_test();
        conn.execute_batch("CREATE TABLE Audit(value INTEGER)")
            .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO Audit VALUES (1)", []).unwrap();
        cloud.state.lock().unwrap().fault = Some(("SELECT count(*) FROM Audit".into(), fault));
        assert!(
            tx.query_row("SELECT count(*) FROM Audit", [], |r| r.get::<_, i64>(0))
                .is_err()
        );
        assert!(tx.execute("INSERT INTO Audit VALUES (2)", []).is_err());
        drop(tx);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM Audit", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        conn.execute("INSERT INTO Audit VALUES (3)", []).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM Audit", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn commit_closes_stream_on_hosted_and_legacy_servers() {
    for close_idle_streams in [true, false] {
        let cloud = Cloud::new();
        cloud.state.lock().unwrap().close_idle_streams = close_idle_streams;
        let db = cloud.database().unwrap();
        let mut conn = db.account_conn_for_test();
        conn.execute_batch("CREATE TABLE Audit(value INTEGER)")
            .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO Audit VALUES (1)", []).unwrap();
        tx.commit().unwrap();
        let state = cloud.state.lock().unwrap();
        assert!(state.streams.is_empty());
        let commit = state.requests.last().unwrap();
        let requests = commit["requests"].as_array().unwrap();
        assert_eq!(requests[0]["sql"], "COMMIT");
        assert_eq!(requests.last().unwrap()["type"], "close");
        drop(state);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM Audit", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(conn);
        let admin = user(&db, "alice");
        drop(db);
        let db = cloud.database().unwrap();
        assert_eq!(db.get_user("alice").unwrap().unwrap().id, admin.id);
        assert!(db.list_networks(admin.id).unwrap().is_empty());
    }
}

#[test]
fn failed_begin_releases_stream_and_ambiguous_commit_is_not_replayed() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let mut conn = db.account_conn_for_test();
    conn.execute_batch("CREATE TABLE Audit(value INTEGER)")
        .unwrap();
    cloud.state.lock().unwrap().fault = Some(("BEGIN IMMEDIATE".into(), Fault::MissingResults));
    assert!(
        conn.transaction_with_behavior(TransactionBehavior::Immediate)
            .is_err()
    );
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute("INSERT INTO Audit VALUES (1)", []).unwrap();
    cloud.state.lock().unwrap().fault = Some(("COMMIT".into(), Fault::HttpAfter));
    assert!(tx.commit().is_err());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM Audit", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let inserts = cloud
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .flat_map(|r| r["requests"].as_array().unwrap())
        .filter(|r| r["stmt"]["sql"].as_str() == Some("INSERT INTO Audit VALUES (1)"))
        .count();
    assert_eq!(inserts, 1);
}

#[test]
fn malformed_commit_responses_fail_without_replaying_writes() {
    for fault in [
        Fault::MissingResults,
        Fault::WrongResponseType,
        Fault::EmptyBody,
    ] {
        let cloud = Cloud::new();
        let db = cloud.database().unwrap();
        let mut conn = db.account_conn_for_test();
        conn.execute_batch("CREATE TABLE Audit(value INTEGER)")
            .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO Audit VALUES (1)", []).unwrap();
        cloud.state.lock().unwrap().fault = Some(("COMMIT".into(), fault));
        assert!(tx.commit().is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM Audit", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let state = cloud.state.lock().unwrap();
        assert!(state.streams.is_empty());
        assert_eq!(
            state
                .requests
                .iter()
                .flat_map(|r| r["requests"].as_array().unwrap())
                .filter(|r| r["sql"] == "COMMIT")
                .count(),
            2 // Controller initialization and the explicit Audit transaction.
        );
    }
}

#[test]
fn binary_values_named_parameters_and_replication_index() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let conn = db.account_conn_for_test();
    conn.execute_batch("CREATE TABLE Audit(payload BLOB)")
        .unwrap();
    let blob = vec![0xff; 64]; // base64 requires padding unless using Hrana's unpadded encoding
    conn.execute("INSERT INTO Audit(payload) VALUES (?)", [&blob])
        .unwrap();
    let expected_index = cloud.state.lock().unwrap().replication_index;
    assert_eq!(
        conn.query_row("SELECT payload FROM Audit", [], |r| r.get::<_, Vec<u8>>(0))
            .unwrap(),
        blob
    );
    let state = cloud.state.lock().unwrap();
    let request = state.requests.last().unwrap();
    assert_eq!(
        request["requests"][1]["stmt"]["replication_index"],
        expected_index.to_string()
    );
    drop(state);
    let values = conn
        .query_row(
            "SELECT :value, @value, $value",
            named_params! {":value":i64::MAX, "@value":i64::MIN, "$value":42},
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(values, (i64::MAX, i64::MIN, 42));
}

#[test]
fn slow_http_request_does_not_block_an_independent_stream() {
    let cloud = Cloud::new();
    let turso = cloud.open().unwrap();
    cloud.state.lock().unwrap().delay = Some(("SELECT 'slow'".into(), Duration::from_millis(400)));
    let url = format!("{}/db/soju-controller/v2/pipeline", cloud.url);
    let http = turso.http.clone();
    let slow_url = url.clone();
    let slow = std::thread::spawn(move || {
        http.json(Method::POST, &slow_url, "sql-secret",
        Some(json!({"requests":[{"type":"execute","stmt":{"sql":"SELECT 'slow'"}}, {"type":"close"}]}))).unwrap()
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !cloud
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|r| r["requests"][0]["stmt"]["sql"] == "SELECT 'slow'")
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = std::time::Instant::now();
    turso.http.json(Method::POST, &url, "sql-secret",
        Some(json!({"requests":[{"type":"execute","stmt":{"sql":"SELECT 'fast'"}}, {"type":"close"}]}))).unwrap();
    assert!(start.elapsed() < Duration::from_millis(250));
    slow.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn slow_storage_does_not_freeze_the_user_runtime_or_user_directory() {
    use crate::logger::Logger;
    use crate::server::{Server, ServerConfig};
    use crate::user::Event;
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    user(&db, "alice");
    let srv = Server::new(db, Logger::new(false));
    let mut config = ServerConfig::default();
    config.basic.msg_store.driver = crate::config::MsgStoreDriver::Turso;
    srv.set_config(config);
    srv.start().unwrap();
    let handle = srv.get_user("alice").unwrap();
    let (ready, wait_ready) = tokio::sync::oneshot::channel();
    handle.send(Event::Run(Box::new(move |_| {
        let _ = ready.send(());
    })));
    wait_ready.await.unwrap();
    cloud.state.lock().unwrap().delay = Some(("UPDATE User".into(), Duration::from_millis(400)));
    let (started, wait_started) = tokio::sync::oneshot::channel();
    let (done, wait_done) = tokio::sync::oneshot::channel();
    let start = std::time::Instant::now();
    handle.send(Event::Run(Box::new(move |user| {
        let _ = started.send(());
        let result = user.update_user(|record| {
            record.nick = "updated".into();
            Ok(())
        });
        let _ = done.send(result);
    })));
    wait_started.await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(start.elapsed() < Duration::from_millis(250));
    wait_done.await.unwrap().unwrap();
    cloud.state.lock().unwrap().delay =
        Some(("CREATE TABLE User".into(), Duration::from_millis(400)));
    let creator = srv.clone();
    let creating = tokio::task::spawn_blocking(move || creator.create_user(User::new("bob")));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let start = std::time::Instant::now();
    assert!(srv.get_user("alice").is_some());
    assert!(start.elapsed() < Duration::from_millis(250));
    creating.await.unwrap().unwrap();
    srv.shutdown().await;
}

#[test]
fn cross_tenant_network_update_cannot_poison_routes_after_restart() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let alice = user(&db, "alice");
    let bob = user(&db, "bob");
    let mut net = Network::new("ircs://bob.example.org");
    db.store_network(bob.id, &mut net).unwrap();
    db.store_messages(
        net.id,
        "#private",
        &[msg!("PRIVMSG", "#private", "bob's history")],
    )
    .unwrap();
    drop(db);
    let db = cloud.database().unwrap(); // ownership cache starts empty
    net.addr = "ircs://wrong.example.org".into();
    assert!(db.store_network(alice.id, &mut net).is_err());
    assert_eq!(
        db.list_networks(bob.id).unwrap()[0].addr,
        "ircs://bob.example.org"
    );
    assert_eq!(
        db.list_messages(
            net.id,
            "#private",
            &MessageOptions {
                limit: 10,
                ..Default::default()
            }
        )
        .unwrap()[0]
            .trailing(),
        "bob's history"
    );
}

#[test]
fn startup_and_recurring_expiry_never_send_file_maintenance_sql() {
    use crate::logger::Logger;
    use crate::server::{Server, ServerConfig};
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    let alice = user(&db, "alice");
    let mut net = Network::new("ircs://example.org");
    db.store_network(alice.id, &mut net).unwrap();
    let srv = Server::new(db, Logger::new(false));
    let mut config = ServerConfig::default();
    config.basic.msg_store.driver = crate::config::MsgStoreDriver::Turso;
    config.basic.message_expiry = Duration::from_secs(86400);
    srv.set_config(config);
    for startup in [true, false] {
        let mut old = msg!("PRIVMSG", "#a", "expired");
        old.tags.set(
            "time",
            crate::xirc::format_server_time(chrono::Utc::now() - chrono::Duration::days(2)),
        );
        let fresh = msg!("PRIVMSG", "#a", "retained");
        srv.db.store_messages(net.id, "#a", &[old, fresh]).unwrap();
        srv.delete_expired_messages(startup).unwrap();
        let messages = srv
            .db
            .list_messages(
                net.id,
                "#a",
                &MessageOptions {
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(messages.iter().all(|m| m.trailing() == "retained"));
        assert_eq!(messages.len(), if startup { 1 } else { 2 });
    }
    // These public no-ops also cannot leak SQLite maintenance into Turso.
    srv.db.vacuum().unwrap();
    srv.db.shrink_memory().unwrap();
    assert_eq!(srv.db.free_page_ratio().unwrap(), 0.0);
    let state = cloud.state.lock().unwrap();
    let sql: Vec<_> = state
        .requests
        .iter()
        .flat_map(|r| r["requests"].as_array().unwrap())
        .filter_map(|r| r["sql"].as_str().or_else(|| r["stmt"]["sql"].as_str()))
        .map(str::to_ascii_lowercase)
        .collect();
    assert!(sql.iter().any(|s| s.contains("delete from message")));
    assert!(
        sql.iter()
            .any(|s| s.contains("insert into messagefts(messagefts) values ('optimize')"))
    );
    for forbidden in [
        "vacuum",
        "wal_checkpoint",
        "shrink_memory",
        "freelist_count",
        "page_count",
        "analyze ",
    ] {
        assert!(!sql.iter().any(|s| s.contains(forbidden)), "{forbidden}");
    }
}

fn buffered_store(
    cloud: &Cloud,
    limit: usize,
) -> (
    Arc<crate::database::Database>,
    User,
    Network,
    crate::msgstore::Store,
) {
    let db = Arc::new(cloud.database().unwrap());
    let account = user(&db, "buffered");
    let mut net = Network::new("ircs://buffered.example.org");
    net.enabled = false;
    db.store_network(account.id, &mut net).unwrap();
    let store = crate::msgstore::Store::Db(crate::msgstore::DbStore::new_turso(
        db.clone(),
        account.id,
        limit,
    ));
    store.prepare().unwrap();
    (db, account, net, store)
}

fn timed_message(text: &str, seconds: i64) -> crate::irc::Message {
    let mut message = msg!("PRIVMSG", "#buffered", text)
        .with_prefix(crate::irc::Prefix::parse("alice!user@host"));
    message.tags.set(
        "time",
        crate::xirc::format_server_time(Utc::now() + chrono::Duration::seconds(seconds)),
    );
    message
}

#[test]
fn buffered_history_search_targets_and_shutdown_include_pending_lines() {
    use crate::msgstore::{LoadMessageOptions, SearchMessageOptions};
    let cloud = Cloud::new();
    let (db, _, net, mut store) = buffered_store(&cloud, 100);
    let zero = store.last_msg_id(&net, "#buffered").unwrap();
    let first = timed_message("alpha one", -10);
    let later = timed_message("alpha two", -5);
    let first_id = store.append(&net, "#buffered", &first).unwrap();
    let later_id = store.append(&net, "#buffered", &later).unwrap();
    assert_eq!(store.last_msg_id(&net, "#buffered").unwrap(), later_id);
    let opts = LoadMessageOptions {
        network: &net,
        entity: "#buffered",
        limit: 100,
        events: false,
    };
    assert_eq!(
        store.load_latest_id(&zero, &opts).unwrap(),
        vec![first.clone(), later.clone()]
    );
    assert_eq!(
        store.load_latest_id(&first_id, &opts).unwrap(),
        vec![later.clone()]
    );
    assert_eq!(
        store
            .search(
                &net,
                &SearchMessageOptions {
                    r#in: "#buffered".into(),
                    text: "alpha".into(),
                    limit: 100,
                    ..Default::default()
                }
            )
            .unwrap(),
        vec![first.clone(), later.clone()]
    );
    let now = Utc::now();
    assert_eq!(
        store
            .list_targets(&net, now - chrono::Duration::hours(1), now, 100, false)
            .unwrap()
            .len(),
        1
    );
    assert!(
        db.list_messages(
            net.id,
            "#buffered",
            &MessageOptions {
                limit: 100,
                ..Default::default()
            }
        )
        .unwrap()
        .is_empty()
    );
    store.close().unwrap();
    assert_eq!(
        db.list_messages(
            net.id,
            "#buffered",
            &MessageOptions {
                limit: 100,
                ..Default::default()
            }
        )
        .unwrap(),
        vec![first, later]
    );
}

#[test]
fn a_slow_commit_does_not_block_the_other_buffer_or_history_reads() {
    use crate::msgstore::LoadMessageOptions;
    let cloud = Cloud::new();
    let (db, _, net, mut store) = buffered_store(&cloud, 3);
    let zero = store.last_msg_id(&net, "#buffered").unwrap();
    cloud.state.lock().unwrap().delay_before =
        Some(("INSERT INTO Message(".into(), Duration::from_millis(500)));
    let start = std::time::Instant::now();
    for i in 0..6 {
        store
            .append(
                &net,
                "#buffered",
                &timed_message(&format!("line {i}"), i - 10),
            )
            .unwrap();
    }
    assert!(start.elapsed() < Duration::from_millis(200));
    assert!(!store.has_capacity());
    assert_eq!(
        store
            .load_latest_id(
                &zero,
                &LoadMessageOptions {
                    network: &net,
                    entity: "#buffered",
                    limit: 100,
                    events: false
                }
            )
            .unwrap()
            .len(),
        6
    );
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "history waited for the writer connection"
    );
    store.close().unwrap();
    let rows = db
        .list_messages(
            net.id,
            "#buffered",
            &MessageOptions {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(rows.len(), 6);
    let state = cloud.state.lock().unwrap();
    let batches: Vec<_> = state
        .requests
        .iter()
        .flat_map(|r| r["requests"].as_array().unwrap())
        .filter(|r| r["type"] == "batch")
        .collect();
    assert_eq!(batches.len(), 2);
    for batch in batches {
        assert_eq!(
            batch["batch"]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["stmt"]["sql"]
                    .as_str()
                    .unwrap()
                    .starts_with("INSERT INTO Message("))
                .count(),
            3
        );
    }
}

#[test]
fn atomic_message_batches_rollback_and_reject_conflicting_reserved_ids() {
    use crate::database::MessageInsert;
    let cloud = Cloud::new();
    let (db, account, net, mut store) = buffered_store(&cloud, 100);
    store.close().unwrap();
    let writer = db.message_writer(account.id).unwrap();
    let mut ids = writer.reserve(3).unwrap();
    let id = ids.next().unwrap();
    let first = MessageInsert::new(id, net.id, "#buffered", &timed_message("original", 0)).unwrap();
    let bad = MessageInsert::new(
        id,
        net.id,
        "#buffered",
        &timed_message("conflicting contents", 0),
    )
    .unwrap();
    assert!(writer.write(&[first.clone(), bad]).is_err());
    let opts = MessageOptions {
        limit: 100,
        ..Default::default()
    };
    assert!(
        db.list_messages(net.id, "#buffered", &opts)
            .unwrap()
            .is_empty()
    );
    writer.write(std::slice::from_ref(&first)).unwrap();
    writer.write(&[first]).unwrap();
    let collision = MessageInsert::new(
        id,
        net.id,
        "#buffered",
        &timed_message("different contents", 0),
    )
    .unwrap();
    assert!(writer.write(&[collision]).is_err());
    let rows = db.list_messages(net.id, "#buffered", &opts).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].params[1], "original");
}

#[test]
fn retry_lost_batch_commit_response_without_duplicate_lines() {
    let cloud = Cloud::new();
    let (db, _, net, mut store) = buffered_store(&cloud, 3);
    cloud.state.lock().unwrap().fault = Some(("COMMIT".into(), Fault::HttpAfter));
    for i in 0..3 {
        store
            .append(&net, "#buffered", &timed_message(&format!("retry {i}"), i))
            .unwrap();
    }
    store.close().unwrap();
    let rows = db
        .list_messages(
            net.id,
            "#buffered",
            &MessageOptions {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(cloud.state.lock().unwrap().streams.len(), 0);
}

#[test]
fn encoded_json_body_budget_splits_long_escaped_lines() {
    let cloud = Cloud::new();
    let (db, account, net, mut store) = buffered_store(&cloud, 100);
    store.close().unwrap();
    let writer = db.message_writer(account.id).unwrap();
    let ids = writer.reserve(20).unwrap();
    let text = "\"\\".repeat(10000);
    let rows: Vec<_> = ids
        .map(|id| {
            crate::database::MessageInsert::new(id, net.id, "#buffered", &timed_message(&text, 0))
                .unwrap()
        })
        .collect();
    cloud.state.lock().unwrap().request_sizes.clear();
    writer.write(&rows).unwrap();
    let state = cloud.state.lock().unwrap();
    assert!(state.request_sizes.len() > 1);
    assert!(
        state
            .request_sizes
            .iter()
            .all(|size| *size <= crate::database::connection::MESSAGE_HTTP_BODY_BUDGET)
    );
    drop(state);
    assert_eq!(
        db.list_messages(
            net.id,
            "#buffered",
            &MessageOptions {
                limit: 100,
                ..Default::default()
            }
        )
        .unwrap()
        .len(),
        20
    );
}

#[test]
fn split_and_retry_host_body_and_processing_limits() {
    for processing in [false, true] {
        let cloud = Cloud::new();
        let (db, _, net, mut store) = buffered_store(&cloud, 10);
        {
            let mut state = cloud.state.lock().unwrap();
            if processing {
                state.max_batch_lines = Some(2);
            } else {
                state.max_body = Some(6000);
            }
        }
        for i in 0..10 {
            store
                .append(
                    &net,
                    "#buffered",
                    &timed_message(&format!("bounded {i}"), i),
                )
                .unwrap();
        }
        store.close().unwrap();
        assert_eq!(
            db.list_messages(
                net.id,
                "#buffered",
                &MessageOptions {
                    limit: 100,
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            10
        );
    }
}

#[test]
fn reserved_message_ids_are_not_reused_after_restart_or_expiry() {
    let cloud = Cloud::new();
    let (db, account, net, mut store) = buffered_store(&cloud, 100);
    let first = store
        .append(&net, "#buffered", &timed_message("old", -10))
        .unwrap();
    store.close().unwrap();
    db.delete_messages_before(Utc::now(), 100).unwrap();
    let mut next = crate::msgstore::Store::Db(crate::msgstore::DbStore::new_turso(
        db.clone(),
        account.id,
        100,
    ));
    next.prepare().unwrap();
    next.append(&net, "#buffered", &timed_message("new", 0))
        .unwrap();
    let history = next
        .load_latest_id(
            &first,
            &crate::msgstore::LoadMessageOptions {
                network: &net,
                entity: "#buffered",
                limit: 100,
                events: false,
            },
        )
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].params[1], "new");
    next.close().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn background_commits_keep_the_same_user_event_loop_responsive() {
    use crate::logger::Logger;
    use crate::server::{Server, ServerConfig};
    use crate::user::Event;
    let cloud = Cloud::new();
    let (db, _, net, mut unused) = buffered_store(&cloud, 100);
    unused.close().unwrap();
    drop(unused);
    let db = Arc::try_unwrap(db)
        .ok()
        .unwrap_or_else(|| panic!("database still referenced"));
    let srv = Server::new(db, Logger::new(false));
    let mut config = ServerConfig::default();
    config.basic.msg_store.driver = crate::config::MsgStoreDriver::Turso;
    config.basic.chat_history_limit = 100;
    srv.set_config(config);
    srv.start().unwrap();
    let handle = srv.get_user("buffered").unwrap();
    let (ready, wait_ready) = tokio::sync::oneshot::channel();
    handle.send(Event::Run(Box::new(move |_| {
        let _ = ready.send(());
    })));
    wait_ready.await.unwrap();
    cloud.state.lock().unwrap().delay =
        Some(("INSERT INTO Message(".into(), Duration::from_millis(700)));
    let (sent, wait_sent) = tokio::sync::oneshot::channel();
    handle.send(Event::Run(Box::new(move |u| {
        for i in 0..200 {
            u.msg_store
                .append(&net, "#buffered", &timed_message(&format!("live {i}"), i))
                .unwrap();
        }
        assert!(!u.msg_store.has_capacity());
        let _ = sent.send(());
    })));
    tokio::time::timeout(Duration::from_millis(350), wait_sent)
        .await
        .unwrap()
        .unwrap();
    let (reply, wait_reply) = tokio::sync::oneshot::channel();
    handle.send(Event::HasNetwork {
        name: "ircs://buffered.example.org".into(),
        reply,
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), wait_reply)
            .await
            .unwrap()
            .unwrap()
    );
    srv.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn partial_message_batches_flush_on_the_timer() {
    use crate::logger::Logger;
    use crate::server::{Server, ServerConfig};
    use crate::user::Event;
    let cloud = Cloud::new();
    let (db, _, net, mut unused) = buffered_store(&cloud, 100);
    unused.close().unwrap();
    drop(unused);
    let db = Arc::try_unwrap(db)
        .ok()
        .unwrap_or_else(|| panic!("database still referenced"));
    let srv = Server::new(db, Logger::new(false));
    let mut config = ServerConfig::default();
    config.basic.msg_store.driver = crate::config::MsgStoreDriver::Turso;
    config.basic.chat_history_limit = 100;
    srv.set_config(config);
    srv.start().unwrap();
    let handle = srv.get_user("buffered").unwrap();
    let net_id = net.id;
    let (sent, wait_sent) = tokio::sync::oneshot::channel();
    handle.send(Event::Run(Box::new(move |u| {
        u.msg_store
            .append(&net, "#buffered", &timed_message("quiet", 0))
            .unwrap();
        let _ = sent.send(());
    })));
    wait_sent.await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let rows = crate::database::run(&srv.db, move |db| {
            db.list_messages(
                net_id,
                "#buffered",
                &MessageOptions {
                    limit: 100,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
        if !rows.is_empty() {
            assert_eq!(rows.len(), 1);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    srv.shutdown().await;
}

#[test]
fn a_deleted_network_is_not_resurrected_by_buffered_writes() {
    let cloud = Cloud::new();
    let (db, account, net, mut store) = buffered_store(&cloud, 100);
    let mut other = Network::new("ircs://other.example.org");
    db.store_network(account.id, &mut other).unwrap();
    store
        .append(&net, "#buffered", &timed_message("deleted", 0))
        .unwrap();
    store
        .append(&other, "#buffered", &timed_message("kept", 0))
        .unwrap();
    db.delete_network(net.id).unwrap();
    store.close().unwrap();
    assert_eq!(db.list_networks(account.id).unwrap().len(), 1);
    assert_eq!(
        db.list_messages(
            other.id,
            "#buffered",
            &MessageOptions {
                limit: 100,
                ..Default::default()
            }
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn a_large_logical_multiline_message_fits_the_encoded_body_budget() {
    let cloud = Cloud::new();
    let (db, _, net, mut store) = buffered_store(&cloud, 1);
    let text = "\u{1}".repeat(64 * 1024);
    store
        .append(&net, "#buffered", &timed_message(&text, 0))
        .unwrap();
    store.close().unwrap();
    assert_eq!(
        db.list_messages(
            net.id,
            "#buffered",
            &MessageOptions {
                limit: 1,
                ..Default::default()
            }
        )
        .unwrap()[0]
            .params[1],
        text
    );
    assert!(
        cloud
            .state
            .lock()
            .unwrap()
            .request_sizes
            .iter()
            .all(|size| *size <= crate::database::connection::MESSAGE_HTTP_BODY_BUDGET)
    );
}

#[test]
fn pending_and_committed_history_merge_in_timestamp_and_id_order() {
    use crate::msgstore::LoadMessageOptions;
    let cloud = Cloud::new();
    let (db, account, net, mut old) = buffered_store(&cloud, 100);
    let zero = old.last_msg_id(&net, "#buffered").unwrap();
    old.close().unwrap();
    drop(old);
    let at = |text: &str, time: &str| msg!("PRIVMSG", "#buffered", text).with_tag("time", time);
    let first = at("first", "2026-10-28T00:00:10.000Z");
    let second = at("second", "2026-10-28T00:00:20.000Z");
    db.store_messages(net.id, "#buffered", &[first.clone(), second.clone()])
        .unwrap();
    let mut store = crate::msgstore::Store::Db(crate::msgstore::DbStore::new_turso(
        db.clone(),
        account.id,
        100,
    ));
    store.prepare().unwrap();
    let early = at("early", "2026-10-28T00:00:05.000Z");
    let tie = at("tie", "2026-10-28T00:00:20.000Z");
    store.append(&net, "#buffered", &early).unwrap();
    store.append(&net, "#buffered", &tie).unwrap();
    let opts = LoadMessageOptions {
        network: &net,
        entity: "#buffered",
        limit: 2,
        events: false,
    };
    assert_eq!(
        store.load_latest_id(&zero, &opts).unwrap(),
        vec![second, tie]
    );
    assert_eq!(
        store.load_after_time(None, None, &opts).unwrap(),
        vec![early, first]
    );
    store.close().unwrap();
}

#[test]
fn failed_background_batches_retain_both_buffers_until_recovery() {
    let cloud = Cloud::new();
    let (db, account, net, mut store) = buffered_store(&cloud, 3);
    let name: String = db
        .account_conn_for_test()
        .query_row(
            "SELECT name FROM UserDatabase WHERE user=?",
            [account.id],
            |r| r.get(0),
        )
        .unwrap();
    cloud.state.lock().unwrap().fail_sql = Some(name);
    for i in 0..6 {
        store
            .append(
                &net,
                "#buffered",
                &timed_message(&format!("preserved {i}"), i),
            )
            .unwrap();
    }
    assert!(!store.has_capacity());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while store.poll().is_ok() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!store.has_capacity());
    cloud.state.lock().unwrap().fail_sql = None;
    store.close().unwrap();
    assert_eq!(
        db.list_messages(
            net.id,
            "#buffered",
            &MessageOptions {
                limit: 100,
                ..Default::default()
            }
        )
        .unwrap()
        .len(),
        6
    );
}

#[test]
fn initialize_an_existing_empty_controller_with_read_only_user_version() {
    let cloud = Cloud::new();
    {
        let mut state = cloud.state.lock().unwrap();
        state
            .databases
            .insert("soju-controller".into(), "soju".into());
        rusqlite::Connection::open(state.root.join("soju-controller")).unwrap();
    }
    let db = cloud.database().unwrap();
    let version: i64 = db
        .account_conn_for_test()
        .query_row(
            "SELECT version FROM SojuSchemaVersion WHERE id=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, 1);
    let admin = user(&db, "alice");
    assert!(db.get_user("alice").unwrap().unwrap().admin);
    assert!(db.list_networks(admin.id).unwrap().is_empty());
    assert!(
        cloud
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .flat_map(|r| r["requests"].as_array().unwrap())
            .all(|r| {
                let sql = r["sql"]
                    .as_str()
                    .or_else(|| r["stmt"]["sql"].as_str())
                    .unwrap_or("")
                    .to_lowercase();
                !sql.contains("pragma user_version =")
            })
    );
}

#[test]
fn existing_legacy_controller_imports_its_schema_version_and_keeps_accounts() {
    let cloud = Cloud::new();
    let db = cloud.database().unwrap();
    user(&db, "alice");
    drop(db);
    let path = cloud.state.lock().unwrap().root.join("soju-controller");
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch("DROP TABLE SojuSchemaVersion; PRAGMA user_version = 1;")
        .unwrap();
    let db = cloud.database().unwrap();
    assert!(db.get_user("alice").unwrap().unwrap().admin);
    assert_eq!(
        db.account_conn_for_test()
            .query_row(
                "SELECT version FROM SojuSchemaVersion WHERE id=0",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
