//! The small synchronous SQL interface shared by SQLite and Turso.
//! SQL, parameter binding and row decoders stay in one place in `sqlite/`.

use std::cell::{Cell, RefCell};
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use rusqlite::types::{FromSql, ToSqlOutput, Value, ValueRef};
use rusqlite::{Params as SqliteParams, Result, ToSql, TransactionBehavior};
use serde_json::{Value as Json, json};

use super::turso::http::Http;

// An application budget, deliberately below typical proxy body limits.
// Count encoded JSON bytes, including parameters and protocol envelopes.
pub(super) const MESSAGE_HTTP_BODY_BUDGET: usize = 512 * 1024;

fn error(e: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(std::io::Error::other(e.to_string()).into())
}

fn encode(v: &dyn ToSql) -> Result<Json> {
    let value = v.to_sql()?;
    let value = match &value {
        ToSqlOutput::Borrowed(v) => *v,
        ToSqlOutput::Owned(v) => v.into(),
        _ => return Err(error("unsupported Turso SQL parameter")),
    };
    Ok(match value {
        ValueRef::Null => json!({"type":"null"}),
        ValueRef::Integer(v) => json!({"type":"integer", "value":v.to_string()}),
        ValueRef::Real(v) => json!({"type":"float", "value":v}),
        ValueRef::Text(v) => json!({"type":"text", "value":String::from_utf8_lossy(v)}),
        ValueRef::Blob(v) => json!({"type":"blob", "base64":STANDARD_NO_PAD.encode(v)}),
    })
}

pub(super) fn statement<P: Params>(sql: &str, params: P) -> Result<Json> {
    let mut stmt = json!({"sql":sql, "want_rows":false});
    params.bind(&mut stmt)?;
    Ok(stmt)
}

fn stream_endpoint(base: &str) -> Result<String> {
    let base = reqwest::Url::parse(base).map_err(error)?;
    if base.scheme() != "https"
        || !base.host_str().is_some_and(|h| h.ends_with(".turso.io"))
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Err(error("invalid Turso stream base URL"));
    }
    Ok(base.join("v2/pipeline").map_err(error)?.into())
}

fn pipeline_result(result: &Json, expected: &str) -> Result<Json> {
    if result["type"] == "error" {
        let code = result["error"]["code"].as_str().unwrap_or("UNKNOWN");
        if code == "SQLITE_CONSTRAINT_UNIQUE" {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
                None,
            ));
        }
        return Err(error(format!(
            "Turso SQL error ({code}): {}",
            result["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
        )));
    }
    if result["type"] != "ok" || result["response"]["type"] != expected {
        return Err(error("invalid Turso SQL response type"));
    }
    Ok(result["response"].clone())
}

pub(super) trait Params: SqliteParams {
    fn bind(self, stmt: &mut Json) -> Result<()>;
}

impl Params for [&(dyn ToSql + Send + Sync); 0] {
    fn bind(self, _: &mut Json) -> Result<()> {
        Ok(())
    }
}

impl<T: ToSql> Params for [T; 1] {
    fn bind(self, stmt: &mut Json) -> Result<()> {
        stmt["args"] = json!([encode(&self[0])?]);
        Ok(())
    }
}

impl Params for &[&dyn ToSql] {
    fn bind(self, stmt: &mut Json) -> Result<()> {
        stmt["args"] = self
            .iter()
            .map(|v| encode(*v))
            .collect::<Result<Vec<_>>>()?
            .into();
        Ok(())
    }
}

impl Params for &[(&str, &dyn ToSql)] {
    fn bind(self, stmt: &mut Json) -> Result<()> {
        stmt["named_args"] = self
            .iter()
            .map(|(name, value)| Ok(json!({"name":name, "value":encode(*value)?})))
            .collect::<Result<Vec<_>>>()?
            .into();
        Ok(())
    }
}

fn decode(v: &Json) -> Result<Value> {
    let invalid = || error("invalid value in Turso SQL response");
    Ok(match v["type"].as_str() {
        Some("null") => Value::Null,
        Some("integer") => Value::Integer(
            v["value"]
                .as_str()
                .ok_or_else(invalid)?
                .parse()
                .map_err(error)?,
        ),
        Some("float") => Value::Real(v["value"].as_f64().ok_or_else(invalid)?),
        Some("text") => Value::Text(v["value"].as_str().ok_or_else(invalid)?.into()),
        Some("blob") => Value::Blob(
            STANDARD_NO_PAD
                .decode(
                    v["base64"]
                        .as_str()
                        .ok_or_else(invalid)?
                        .trim_end_matches('='),
                )
                .map_err(error)?,
        ),
        _ => return Err(invalid()),
    })
}

pub(super) enum Row<'a> {
    Local(&'a rusqlite::Row<'a>),
    Remote(&'a [Value]),
}

pub(super) trait ColumnIndex {
    fn index(self) -> usize;
}
impl ColumnIndex for usize {
    fn index(self) -> usize {
        self
    }
}

impl Row<'_> {
    pub fn get<I: ColumnIndex, T: FromSql>(&self, index: I) -> Result<T> {
        let index = index.index();
        match self {
            Self::Local(r) => r.get(index),
            Self::Remote(_) => T::column_result(self.get_ref(index)?).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    self.get_ref(index).unwrap().data_type(),
                    e.into(),
                )
            }),
        }
    }

    pub fn get_ref(&self, index: usize) -> Result<ValueRef<'_>> {
        match self {
            Self::Local(r) => r.get_ref(index),
            Self::Remote(values) => values
                .get(index)
                .map(ValueRef::from)
                .ok_or(rusqlite::Error::InvalidColumnIndex(index)),
        }
    }
}

pub(super) struct Remote {
    http: Http,
    url: String,
    stream_url: RefCell<String>,
    token: String,
    baton: RefCell<Option<String>>,
    transaction: Cell<bool>,
    transaction_failed: Cell<bool>,
    replication_index: Arc<Mutex<Option<u64>>>,
    last_id: Cell<i64>,
    foreign_keys: Cell<bool>,
}

impl Remote {
    fn request(&self, request: Json) -> Result<Json> {
        self.request_with_stream(request, self.transaction.get())
    }

    fn request_with_stream(&self, mut request: Json, keep_stream: bool) -> Result<Json> {
        if self.transaction_failed.get() {
            return Err(error(
                "Turso transaction failed; discard it before issuing more SQL",
            ));
        }
        if let Some(index) = *self.replication_index.lock().unwrap() {
            let target = if request["type"] == "execute" {
                &mut request["stmt"]
            } else if request["type"] == "batch" {
                &mut request["batch"]
            } else {
                &mut request
            };
            target["replication_index"] = index.to_string().into();
        }
        let result = self.pipeline(request, keep_stream);
        if result.is_err() {
            if self.transaction.get() {
                // A failed or malformed response invalidates this transaction.
                // Never continue its SQL on a new auto-commit stream.
                self.transaction_failed.set(true);
            } else {
                self.discard_stream();
            }
        }
        result
    }

    fn pipeline(&self, request: Json, keep: bool) -> Result<Json> {
        let sequence = request["type"] == "sequence";
        let expected = request["type"]
            .as_str()
            .ok_or_else(|| error("invalid Turso request type"))?;
        let mut requests = Vec::new();
        if self.baton.borrow().is_none() {
            requests.push(json!({"type":"sequence", "sql":format!("PRAGMA foreign_keys = {}", i32::from(self.foreign_keys.get()))}));
        }
        let result_index = requests.len();
        requests.push(request.clone());
        // Sequence responses have no replication index. Read it on the same
        // stream before closing, so the next stream observes schema writes.
        if sequence {
            requests.push(json!({"type":"execute", "stmt":{"sql":"SELECT 1"}}));
        }
        if !keep && expected != "close" {
            requests.push(json!({"type":"close"}));
        }
        let body = json!({"baton":*self.baton.borrow(), "requests":requests});
        if expected == "batch"
            && serde_json::to_vec(&body).map_err(error)?.len() > MESSAGE_HTTP_BODY_BUDGET
        {
            return Err(error("Turso message batch exceeds HTTP body budget"));
        }
        let response = self
            .http
            .json(
                reqwest::Method::POST,
                &self.stream_url.borrow(),
                &self.token,
                Some(body),
            )
            .map_err(error)?;
        let baton = match response.get("baton") {
            Some(Json::Null) => None,
            Some(Json::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => return Err(error("invalid or missing Turso stream baton")),
        };
        *self.baton.borrow_mut() = baton;
        if let Some(base) = response["base_url"].as_str() {
            *self.stream_url.borrow_mut() = stream_endpoint(base)?;
        } else if !response["base_url"].is_null() {
            return Err(error("invalid Turso stream base URL"));
        }
        let results = response["results"]
            .as_array()
            .ok_or_else(|| error("missing Turso pipeline results"))?;
        if results.len() != requests.len() {
            return Err(error("incomplete Turso pipeline response"));
        }
        for (result, request) in results.iter().zip(&requests) {
            let payload = pipeline_result(result, request["type"].as_str().unwrap())?;
            if let Some(index) = payload["result"]["replication_index"].as_str() {
                let index = index.parse::<u64>().map_err(error)?;
                let mut last = self.replication_index.lock().unwrap();
                *last = Some(last.map_or(index, |last| last.max(index)));
            }
        }
        if keep && self.baton.borrow().is_none() {
            return Err(error("Turso closed the stream during a transaction"));
        }
        if !keep && self.baton.borrow().is_some() {
            return Err(error("Turso did not close the SQL stream"));
        }
        if self.baton.borrow().is_none() {
            *self.stream_url.borrow_mut() = self.url.clone();
        }
        pipeline_result(&results[result_index], expected)
    }

    /// Closing a stream rolls back any open transaction. Use the last known
    /// baton once, then forget it even if the server no longer recognizes it.
    fn discard_stream(&self) {
        let baton = self.baton.borrow_mut().take();
        if let Some(baton) = baton {
            let _ = self.http.json(
                reqwest::Method::POST,
                &self.stream_url.borrow(),
                &self.token,
                Some(json!({"baton":baton, "requests":[{"type":"close"}]})),
            );
        }
        *self.stream_url.borrow_mut() = self.url.clone();
    }

    fn execute<P: Params>(&self, sql: &str, params: P) -> Result<Json> {
        let mut stmt = json!({"sql":sql, "want_rows":true});
        params.bind(&mut stmt)?;
        let response = self.request(json!({"type":"execute", "stmt":stmt}))?;
        let result = &response["result"];
        if let Some(id) = result["last_insert_rowid"].as_str() {
            self.last_id.set(id.parse().map_err(error)?);
        }
        if !result.is_object() {
            return Err(error("missing Turso SQL result"));
        }
        Ok(result.clone())
    }
}

pub(super) enum Connection {
    Local(rusqlite::Connection),
    Remote(Remote),
}

impl Connection {
    pub fn remote(http: Http, url: String, token: String) -> Self {
        Self::Remote(Remote {
            http,
            stream_url: RefCell::new(url.clone()),
            url,
            token,
            baton: RefCell::new(None),
            transaction: Cell::new(false),
            transaction_failed: Cell::new(false),
            replication_index: Arc::new(Mutex::new(None)),
            last_id: Cell::new(0),
            foreign_keys: Cell::new(true),
        })
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    pub fn initialize_remote_schema_version(&self) -> Result<()> {
        self.execute_batch(
            "CREATE TABLE IF NOT EXISTS SojuSchemaVersion (
            id INTEGER PRIMARY KEY CHECK(id=0), version INTEGER NOT NULL)",
        )?;
        let rows: i64 =
            self.query_row("SELECT count(*) FROM SojuSchemaVersion", [], |r| r.get(0))?;
        if rows == 0 {
            // Read legacy installations without writing the hosted read-only
            // pragma. New databases have user_version=0.
            let legacy: i64 = self.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            self.execute(
                "INSERT INTO SojuSchemaVersion(id, version) VALUES(0, ?) ON CONFLICT DO NOTHING",
                [legacy],
            )?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        let sql = if self.is_remote() {
            "SELECT version FROM SojuSchemaVersion WHERE id=0"
        } else {
            "PRAGMA user_version"
        };
        self.query_row(sql, [], |r| r.get(0))
    }

    pub fn set_schema_version(&self, version: i64) -> Result<()> {
        if self.is_remote() {
            self.execute(
                "UPDATE SojuSchemaVersion SET version=? WHERE id=0",
                [version],
            )?;
        } else {
            self.execute_batch(&format!("PRAGMA user_version = {version}"))?;
        }
        Ok(())
    }

    /// An independent HTTP stream for background message writes. It shares
    /// the read-your-writes watermark, never the read connection's mutex.
    pub fn fork_remote(&self) -> Result<Self> {
        let Self::Remote(c) = self else {
            return Err(error("background message writer requires Turso"));
        };
        let Self::Remote(mut fork) = Self::remote(c.http.clone(), c.url.clone(), c.token.clone())
        else {
            unreachable!()
        };
        fork.replication_index = c.replication_index.clone();
        Ok(Self::Remote(fork))
    }

    /// Execute an atomic batch in one HTTP round trip. Every step depends
    /// on the preceding step succeeding, so an error skips COMMIT. Closing
    /// the stream rolls back any unfinished transaction.
    pub fn atomic_batch(&self, statements: Vec<Json>) -> Result<()> {
        let Self::Remote(c) = self else {
            return Err(error("HTTP batches require Turso"));
        };
        let mut steps = vec![json!({"stmt":{"sql":"BEGIN IMMEDIATE"}})];
        for stmt in statements {
            steps.push(json!({"condition":{"type":"ok", "step":steps.len()-1}, "stmt":stmt}));
        }
        steps.push(
            json!({"condition":{"type":"ok", "step":steps.len()-1}, "stmt":{"sql":"COMMIT"}}),
        );
        let n = steps.len();
        let response = c.request(json!({"type":"batch", "batch":{"steps":steps}}))?;
        let results = response["result"]["step_results"]
            .as_array()
            .filter(|r| r.len() == n)
            .ok_or_else(|| error("incomplete Turso batch results"))?;
        let errors = response["result"]["step_errors"]
            .as_array()
            .filter(|r| r.len() == n)
            .ok_or_else(|| error("incomplete Turso batch errors"))?;
        for (result, err) in results.iter().zip(errors) {
            if !err.is_null() {
                let foreign_key = err["code"] == "SQLITE_CONSTRAINT_FOREIGNKEY"
                    || err["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("FOREIGN KEY constraint failed"));
                return Err(error(format!(
                    "Turso message batch failed ({})",
                    if foreign_key {
                        "SQLITE_CONSTRAINT_FOREIGNKEY"
                    } else {
                        err["code"].as_str().unwrap_or("UNKNOWN")
                    }
                )));
            }
            if !result.is_object() {
                return Err(error("Turso message batch skipped a statement"));
            }
        }
        Ok(())
    }

    pub fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        match self {
            Self::Local(c) => c.execute(sql, params),
            Self::Remote(c) => c.execute(sql, params)?["affected_row_count"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| error("invalid Turso affected row count")),
        }
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match self {
            Self::Local(c) => c.execute_batch(sql),
            Self::Remote(c) => {
                c.request(json!({"type":"sequence", "sql":sql}))?;
                Ok(())
            }
        }
    }

    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        Ok(match self {
            Self::Local(c) => Statement::Local(c.prepare(sql)?),
            Self::Remote(c) => Statement::Remote(c, sql.to_owned()),
        })
    }

    pub fn prepare_cached(&self, sql: &str) -> Result<Statement<'_>> {
        Ok(match self {
            Self::Local(c) => Statement::Cached(c.prepare_cached(sql)?),
            Self::Remote(c) => Statement::Remote(c, sql.to_owned()),
        })
    }

    pub fn query_row<P: Params, T>(
        &self,
        sql: &str,
        params: P,
        mut f: impl FnMut(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Local(c) => c.query_row(sql, params, |r| f(&Row::Local(r))),
            Self::Remote(_) => self
                .prepare(sql)?
                .query_map(params, f)?
                .next()
                .unwrap_or(Err(rusqlite::Error::QueryReturnedNoRows)),
        }
    }

    pub fn last_insert_rowid(&self) -> i64 {
        match self {
            Self::Local(c) => c.last_insert_rowid(),
            Self::Remote(c) => c.last_id.get(),
        }
    }

    pub fn pragma_update(&self, db: Option<&str>, name: &str, value: impl ToSql) -> Result<()> {
        match self {
            Self::Local(c) => c.pragma_update(db, name, value),
            Self::Remote(c) => {
                let integer = match value.to_sql()? {
                    ToSqlOutput::Borrowed(ValueRef::Integer(n))
                    | ToSqlOutput::Owned(Value::Integer(n)) => n,
                    _ => return Err(error("unsupported remote pragma value")),
                };
                if name == "foreign_keys" {
                    c.foreign_keys.set(integer != 0);
                    return Ok(());
                }
                self.execute_batch(&format!(
                    "PRAGMA {name} = {}",
                    match value.to_sql()? {
                        ToSqlOutput::Borrowed(ValueRef::Integer(n)) => n.to_string(),
                        ToSqlOutput::Owned(Value::Integer(n)) => n.to_string(),
                        _ => return Err(error("unsupported remote pragma value")),
                    }
                ))
            }
        }
    }

    pub fn transaction_with_behavior(
        &mut self,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>> {
        if let Self::Remote(c) = self
            && c.transaction.replace(true)
        {
            return Err(error("nested Turso transaction"));
        }
        let begin = match behavior {
            TransactionBehavior::Deferred => "BEGIN DEFERRED",
            TransactionBehavior::Exclusive => "BEGIN EXCLUSIVE",
            _ => "BEGIN IMMEDIATE",
        };
        let result = match self {
            Self::Remote(c) => c.execute(begin, []).map(|_| ()),
            Self::Local(c) => c.execute_batch(begin),
        };
        if let Err(e) = result {
            if let Self::Remote(c) = self {
                c.discard_stream();
                c.transaction.set(false);
                c.transaction_failed.set(false);
            }
            return Err(e);
        }
        Ok(Transaction {
            conn: self,
            committed: false,
        })
    }
}

pub(super) struct Transaction<'a> {
    conn: &'a Connection,
    committed: bool,
}
impl Deref for Transaction<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
    }
}
impl Transaction<'_> {
    pub fn commit(mut self) -> Result<()> {
        match self.conn {
            Connection::Local(c) => c.execute_batch("COMMIT")?,
            Connection::Remote(c) => {
                // End the stream in the COMMIT pipeline. Hosted Turso can
                // already release it once COMMIT succeeds, so a null baton
                // here is expected. Keep transaction poisoning active until
                // every response has been validated; never replay COMMIT.
                c.request_with_stream(
                    json!({"type":"execute", "stmt":{"sql":"COMMIT","want_rows":false}}),
                    false,
                )?;
            }
        }
        self.committed = true;
        self.close();
        Ok(())
    }
    fn close(&self) {
        if let Connection::Remote(c) = self.conn {
            c.discard_stream();
            c.transaction.set(false);
            c.transaction_failed.set(false);
        }
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            if matches!(self.conn, Connection::Local(_)) {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
            self.close();
        }
    }
}

pub(super) enum Statement<'a> {
    Local(rusqlite::Statement<'a>),
    Cached(rusqlite::CachedStatement<'a>),
    Remote(&'a Remote, String),
}

impl Statement<'_> {
    pub fn execute<P: Params>(&mut self, params: P) -> Result<usize> {
        match self {
            Self::Local(s) => s.execute(params),
            Self::Cached(s) => s.execute(params),
            Self::Remote(c, sql) => c.execute(sql, params)?["affected_row_count"]
                .as_u64()
                .map(|n| n as usize)
                .ok_or_else(|| error("invalid Turso affected row count")),
        }
    }

    pub fn query_map<P: Params, T>(
        &mut self,
        params: P,
        mut f: impl FnMut(&Row<'_>) -> Result<T>,
    ) -> Result<std::vec::IntoIter<Result<T>>> {
        let values: Vec<Result<T>> = match self {
            Self::Local(s) => s.query_map(params, |r| f(&Row::Local(r)))?.collect(),
            Self::Cached(s) => s.query_map(params, |r| f(&Row::Local(r)))?.collect(),
            Self::Remote(c, sql) => {
                let result = c.execute(sql, params)?;
                let rows = result["rows"]
                    .as_array()
                    .ok_or_else(|| error("invalid Turso rows"))?;
                rows.iter()
                    .map(|row| {
                        let values = row
                            .as_array()
                            .ok_or_else(|| error("invalid Turso row"))?
                            .iter()
                            .map(decode)
                            .collect::<Result<Vec<_>>>()?;
                        f(&Row::Remote(&values))
                    })
                    .collect()
            }
        };
        Ok(values.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_redirect_cannot_expose_the_group_token_to_an_untrusted_origin() {
        assert_eq!(
            stream_endpoint("https://primary-example.turso.io/").unwrap(),
            "https://primary-example.turso.io/v2/pipeline"
        );
        for url in [
            "http://primary-example.turso.io/",
            "https://example.com/",
            "https://example.turso.io.evil.test/",
            "https://user:secret@primary-example.turso.io/",
        ] {
            assert!(stream_endpoint(url).is_err(), "{url}");
        }
    }
}
