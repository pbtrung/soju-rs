//! The small synchronous SQL interface shared by SQLite and Turso.
//! SQL, parameter binding and row decoders stay in one place in `sqlite/`.

use std::cell::{Cell, RefCell};
use std::ops::Deref;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rusqlite::types::{FromSql, ToSqlOutput, Value, ValueRef};
use rusqlite::{Params as SqliteParams, Result, ToSql, TransactionBehavior};
use serde_json::{Value as Json, json};

use super::turso::http::Http;

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
        ValueRef::Blob(v) => json!({"type":"blob", "base64":STANDARD.encode(v)}),
    })
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
        stmt["named_args"] = self.iter().map(|(name, value)| {
            Ok(json!({"name":name.trim_start_matches([':', '@', '$']), "value":encode(*value)?}))
        }).collect::<Result<Vec<_>>>()?.into();
        Ok(())
    }
}

pub(super) fn decode(v: &Json) -> Result<Value> {
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
            STANDARD
                .decode(v["base64"].as_str().ok_or_else(invalid)?)
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
    last_id: Cell<i64>,
    foreign_keys: Cell<bool>,
}

impl Remote {
    fn request(&self, request: Json) -> Result<Json> {
        let keep = self.transaction.get();
        let mut requests = Vec::new();
        if self.baton.borrow().is_none() {
            requests.push(json!({"type":"sequence", "sql":format!("PRAGMA foreign_keys = {}", i32::from(self.foreign_keys.get()))}));
        }
        let result_index = requests.len();
        let closing = request["type"] == "close";
        requests.push(request);
        if !keep && !closing {
            requests.push(json!({"type":"close"}));
        }
        let response = self
            .http
            .json(
                reqwest::Method::POST,
                &self.stream_url.borrow(),
                &self.token,
                Some(json!({"baton":*self.baton.borrow(), "requests":requests})),
            )
            .map_err(error);
        let response = response?;
        *self.baton.borrow_mut() = response["baton"].as_str().map(str::to_owned);
        if let Some(base) = response["base_url"].as_str() {
            let base = reqwest::Url::parse(base).map_err(error)?;
            if base.scheme() != "https"
                || !base.host_str().is_some_and(|h| h.ends_with(".turso.io"))
                || !base.username().is_empty()
                || base.password().is_some()
            {
                return Err(error("invalid Turso stream base URL"));
            }
            *self.stream_url.borrow_mut() = base.join("v2/pipeline").map_err(error)?.into();
        }
        if self.baton.borrow().is_none() {
            *self.stream_url.borrow_mut() = self.url.clone();
        }
        for setup in response["results"]
            .as_array()
            .ok_or_else(|| error("missing Turso pipeline results"))?
            .iter()
            .take(result_index)
        {
            if setup["type"] != "ok" {
                return Err(error("failed to configure Turso SQL connection"));
            }
        }
        let result = &response["results"][result_index];
        if result["type"] == "error" {
            let code = result["error"]["code"].as_str().unwrap_or("UNKNOWN");
            // Preserve the unique-constraint error used for device certificates.
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
        if result["type"] != "ok" {
            return Err(error("invalid Turso pipeline response"));
        }
        Ok(result["response"].clone())
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
            last_id: Cell::new(0),
            foreign_keys: Cell::new(true),
        })
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
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
        if let Err(e) = self.execute_batch(begin) {
            if let Self::Remote(c) = self {
                c.transaction.set(false);
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
        self.conn.execute_batch("COMMIT")?;
        self.committed = true;
        self.close();
        Ok(())
    }
    fn close(&self) {
        if let Connection::Remote(c) = self.conn {
            c.transaction.set(false);
            if c.baton.borrow().is_some() {
                // Closing releases the stream; a failed commit is never retried.
                let _ = c.request(json!({"type":"close"}));
            }
            *c.baton.borrow_mut() = None;
            *c.stream_url.borrow_mut() = c.url.clone();
        }
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.conn.execute_batch("ROLLBACK");
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
