//! Turso's controller directory and recoverable per-user provisioning.

pub(super) mod http;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode};
use rusqlite::{OptionalExtension, TransactionBehavior, named_params, params};
use serde_json::json;

use super::{User, connection::Connection, sqlite};
use crate::config;

pub(super) struct Turso {
    config: config::Turso,
    http: http::Http,
    pub controller: Arc<sqlite::Database>,
    users: Mutex<HashMap<i64, Arc<sqlite::Database>>>,
    routes: Mutex<HashMap<(&'static str, i64), i64>>,
    api_base: String,
    sql_base: Option<String>,
}

/// Fixed-width base36 encoding of 128 random bits. Padding retains the full
/// entropy, including leading zeroes, and all 25 characters are a-z or 0-9.
fn database_name() -> String {
    let mut bytes = [0; 16];
    rand::fill(&mut bytes);
    let mut n = u128::from_be_bytes(bytes);
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut name = [b'0'; 25];
    for ch in name.iter_mut().rev() {
        *ch = alphabet[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(name.to_vec()).unwrap()
}

#[cfg(test)]
mod tests;

impl Turso {
    pub fn open(config: &config::Turso) -> Result<Self> {
        Self::open_at(config, "https://api.turso.tech/v1".into(), None)
    }

    fn open_at(config: &config::Turso, api_base: String, sql_base: Option<String>) -> Result<Self> {
        let http = http::Http::new()?;
        let url = Self::ensure_database(
            &http,
            config,
            &api_base,
            sql_base.as_deref(),
            &config.controller,
        )?;
        let controller = Arc::new(
            sqlite::Database::open_remote(
                Connection::remote(http.clone(), url, config.group_token.clone()),
                true,
            )
            .context("initialize Turso controller")?,
        );
        let cloud = Self {
            config: config.clone(),
            http,
            controller,
            users: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            api_base,
            sql_base,
        };
        let links: Vec<(i64, String, String)> = cloud
            .controller
            .conn()
            .prepare("SELECT user, name, state FROM UserDatabase")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, name, state) in links {
            if state == "deleting" {
                cloud.delete_user(id)?;
            } else {
                cloud.provision(id, &name, state == "provisioning")?;
            }
        }
        Ok(cloud)
    }

    fn ensure_database(
        http: &http::Http,
        cfg: &config::Turso,
        api_base: &str,
        sql_base: Option<&str>,
        name: &str,
    ) -> Result<String> {
        let collection = format!("{api_base}/organizations/{}/databases", cfg.organization);
        let url = format!("{collection}/{name}");
        let (status, mut value) = http.request(Method::GET, &url, &cfg.api_token, None)?;
        if status == StatusCode::NOT_FOUND {
            let (created, _) = http.request(
                Method::POST,
                &collection,
                &cfg.api_token,
                Some(json!({"name":name, "group":cfg.group})),
            )?;
            // A concurrent starter may have created the same controller.
            if !created.is_success() && created != StatusCode::CONFLICT {
                bail!("create Turso database {name}: HTTP {created}");
            }
            value = http.json(Method::GET, &url, &cfg.api_token, None)?;
        } else if !status.is_success() {
            bail!("retrieve Turso database {name}: HTTP {status}");
        }
        let db = &value["database"];
        if db["group"].as_str() != Some(cfg.group.as_str()) {
            bail!(
                "Turso database {name} does not belong to configured group {}",
                cfg.group
            );
        }
        let host = db["Hostname"]
            .as_str()
            .ok_or_else(|| anyhow!("Turso database {name} has no hostname"))?;
        let endpoint = if let Some(base) = sql_base {
            format!("{base}/db/{name}/v2/pipeline")
        } else {
            if !host.ends_with(".turso.io")
                || !host
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
            {
                bail!("invalid Turso database hostname");
            }
            format!("https://{host}/v2/pipeline")
        };
        // A new database may not be ready to serve SQL immediately. Retry only
        // this read; writes are never replayed after an ambiguous HTTP failure.
        let probe =
            json!({"requests":[{"type":"execute", "stmt":{"sql":"SELECT 1"}}, {"type":"close"}]});
        for attempt in 0..6 {
            let response = http.request(
                Method::POST,
                &endpoint,
                &cfg.group_token,
                Some(probe.clone()),
            )?;
            if response.0.is_success() && response.1["results"][0]["type"] == "ok" {
                return Ok(endpoint);
            }
            if response.0 == StatusCode::UNAUTHORIZED || response.0 == StatusCode::FORBIDDEN {
                bail!("Turso SQL authorization failed for database {name}");
            }
            if attempt == 5 {
                bail!(
                    "Turso database {name} did not become ready (HTTP {})",
                    response.0
                );
            }
            std::thread::sleep(Duration::from_millis(100 * (1 << attempt)));
        }
        unreachable!()
    }

    fn provision(&self, id: i64, name: &str, pending: bool) -> Result<()> {
        let url = Self::ensure_database(
            &self.http,
            &self.config,
            &self.api_base,
            self.sql_base.as_deref(),
            name,
        )?;
        let db = Arc::new(
            sqlite::Database::open_remote(
                Connection::remote(self.http.clone(), url, self.config.group_token.clone()),
                false,
            )
            .with_context(|| format!("initialize user database {name}"))?,
        );
        // User is only a foreign-key anchor here. Passwords stay exclusively
        // in the controller; all network and message data stays in this DB.
        db.conn().execute("INSERT INTO User(id, username, created_at) VALUES (?, ?, ?) ON CONFLICT(id) DO NOTHING",
            params![id, format!("user-{id}"), crate::xirc::format_server_time(chrono::Utc::now())])?;
        self.users.lock().unwrap().insert(id, db);
        if pending {
            self.controller.conn().execute(
                "UPDATE UserDatabase SET state = 'ready' WHERE user = ?",
                [id],
            )?;
        }
        Ok(())
    }

    pub fn ready(&self, id: i64) -> Result<bool> {
        Ok(self.controller.conn().query_row(
            "SELECT state = 'ready' FROM UserDatabase WHERE user = ?",
            [id],
            |r| r.get::<_, bool>(0),
        )?)
    }

    pub fn create_user(&self, user: &mut User) -> Result<()> {
        let name = database_name();
        {
            let mut conn = self.controller.conn();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute("INSERT INTO User(username, password, admin, nick, realname, created_at, enabled, downstream_interacted_at, max_networks)
                VALUES (:username, :password, :admin, :nick, :realname, :now, :enabled, :interacted, :max_networks)",
                named_params! {":username":user.username, ":password":user.password, ":admin":user.admin,
                    ":nick":user.nick, ":realname":user.realname, ":now":crate::xirc::format_server_time(chrono::Utc::now()),
                    ":enabled":user.enabled, ":interacted":user.downstream_interacted_at.map(crate::xirc::format_server_time), ":max_networks":user.max_networks})?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO UserDatabase(user, name, state) VALUES (?, ?, 'provisioning')",
                params![id, name],
            )?;
            tx.commit()?;
            user.id = id;
        }
        self.provision(user.id, &name, true)
            .context("user reserved; restart soju to resume database provisioning")
    }

    pub fn user(&self, id: i64) -> Result<Arc<sqlite::Database>> {
        self.users
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown Turso user {id}"))
    }

    pub fn databases(&self) -> Vec<Arc<sqlite::Database>> {
        self.users.lock().unwrap().values().cloned().collect()
    }

    pub fn remember_network(&self, user: i64, network: i64) -> Result<()> {
        let mut routes = self.routes.lock().unwrap();
        if let Some(owner) = routes.get(&("Network", network))
            && *owner != user
        {
            bail!("duplicate Network ID across Turso user databases");
        }
        routes.insert(("Network", network), user);
        Ok(())
    }

    /// IDs for networks, channels and subscriptions are random 63-bit row IDs
    /// in Turso. Cache ownership; discover it from the tenant DB after restart.
    pub fn route(&self, table: &'static str, id: i64) -> Result<Arc<sqlite::Database>> {
        if let Some(user) = self.routes.lock().unwrap().get(&(table, id)).copied() {
            return self.user(user);
        }
        let users: Vec<_> = self
            .users
            .lock()
            .unwrap()
            .iter()
            .map(|(id, db)| (*id, db.clone()))
            .collect();
        let mut found = None;
        for (user, db) in users {
            let exists = db
                .conn()
                .query_row(&format!("SELECT id FROM {table} WHERE id = ?"), [id], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
                .is_some();
            if exists {
                if found.is_some() {
                    bail!("duplicate {table} ID across Turso user databases");
                }
                found = Some((user, db));
            }
        }
        let (user, db) = found.ok_or_else(|| anyhow!("unknown {table} ID {id}"))?;
        self.routes.lock().unwrap().insert((table, id), user);
        Ok(db)
    }

    pub fn delete_user(&self, id: i64) -> Result<()> {
        let name: String = self.controller.conn().query_row(
            "SELECT name FROM UserDatabase WHERE user = ?",
            [id],
            |r| r.get(0),
        )?;
        // Persist the tombstone before deleting anything. Startup completes a
        // failed deletion instead of recreating the user's database.
        self.controller.conn().execute(
            "UPDATE UserDatabase SET state = 'deleting' WHERE user = ?",
            [id],
        )?;
        let (status, _) = self.http.request(
            Method::DELETE,
            &format!(
                "{}/organizations/{}/databases/{name}",
                self.api_base, self.config.organization
            ),
            &self.config.api_token,
            None,
        )?;
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            bail!("delete Turso user database: HTTP {status}");
        }
        let mut conn = self.controller.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM DeviceCertificate WHERE user = ?", [id])?;
        tx.execute("DELETE FROM UserDatabase WHERE user = ?", [id])?;
        tx.execute("DELETE FROM User WHERE id = ?", [id])?;
        tx.commit()?;
        self.users.lock().unwrap().remove(&id);
        self.routes.lock().unwrap().retain(|_, user| *user != id);
        Ok(())
    }
}
