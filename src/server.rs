//! The bouncer server: users, configuration, admin connections and
//! background tasks (port of upstream's server.go).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::auth::Authenticator;
use crate::config;
use crate::database::{self as db, Database};
use crate::identd::Identd;
use crate::irc::Prefix;
use crate::logf;
use crate::logger::Logger;
use crate::metrics::Metrics;
use crate::user::{Event, User, UserHandle};
use crate::webpush;

mod admin;
mod configuration;
mod maintenance;

pub use configuration::load_server_config;

pub const DOWNSTREAM_REGISTER_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs synchronous code that may block on SQLite calls (e.g. a user's
/// event handlers). On a multi-threaded runtime, the runtime moves its
/// other tasks to another worker meanwhile. A current-thread runtime has no
/// other worker: the code just runs.
pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    use tokio::runtime::{Handle, RuntimeFlavor};
    if Handle::current().runtime_flavor() == RuntimeFlavor::MultiThread {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// The run-time configuration, reloadable on SIGHUP.
#[derive(Clone)]
pub struct ServerConfig {
    pub basic: config::BasicServer,
    pub icon_path: String,
    pub icon_url: String,
    pub motd: String,
    pub auth: Authenticator,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        let mut basic = config::Server::defaults().basic;
        basic.hostname = "localhost".to_owned();
        ServerConfig {
            basic,
            icon_path: String::new(),
            icon_url: String::new(),
            motd: String::new(),
            auth: Authenticator::internal(),
        }
    }
}

pub struct ServerStats {
    pub users: usize,
    pub downstreams: i64,
    pub upstreams: i64,
}

pub struct Server {
    pub logger: Logger,
    pub db: Arc<Database>,
    pub metrics: Metrics,
    config: RwLock<Arc<ServerConfig>>,
    identd: OnceLock<Arc<Identd>>,
    web_push: OnceLock<Arc<webpush::Sender>>,
    users: Mutex<HashMap<String, UserHandle>>,
    user_creation: Mutex<()>,
    shutdown: AtomicBool,
    stop: CancellationToken,
    next_downstream_id: AtomicU64,
    /// Shared with the TLS client certificate verifier
    pub client_cert_auth: Arc<AtomicBool>,
}

impl Server {
    pub fn new(db: Database, logger: Logger) -> Arc<Server> {
        Arc::new(Server {
            logger,
            db: Arc::new(db),
            metrics: Metrics::default(),
            config: RwLock::new(Arc::new(ServerConfig::default())),
            identd: OnceLock::new(),
            web_push: OnceLock::new(),
            users: Mutex::new(HashMap::new()),
            user_creation: Mutex::new(()),
            shutdown: AtomicBool::new(false),
            stop: CancellationToken::new(),
            next_downstream_id: AtomicU64::new(1),
            client_cert_auth: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn config(&self) -> Arc<ServerConfig> {
        self.config.read().unwrap().clone()
    }

    pub fn set_config(&self, cfg: ServerConfig) {
        self.client_cert_auth
            .store(cfg.basic.client_cert_auth, Ordering::Relaxed);
        *self.config.write().unwrap() = Arc::new(cfg);
    }

    /// Applies a reloaded configuration. The message store can't be
    /// changed at run time: users keep the store they were started with, so
    /// the setting in effect is kept until restart. A changed message-expiry
    /// applies right away, vacuuming local SQLite: users wait on the
    /// database meanwhile.
    pub fn reload_config(self: &Arc<Self>, mut cfg: ServerConfig) {
        let cur = self.config();
        if cfg.basic.msg_store != cur.basic.msg_store {
            logf!(
                self.logger,
                "message-store changed: restart to apply the new setting"
            );
            cfg.basic.msg_store = cur.basic.msg_store.clone();
        }
        let expiry_changed = cfg.basic.message_expiry != cur.basic.message_expiry;
        self.set_config(cfg);
        if expiry_changed {
            let srv = self.clone();
            tokio::spawn(async move { srv.run_message_expiry(true).await });
        }
    }

    pub fn prefix(&self) -> Prefix {
        Prefix::new(self.config().basic.hostname.clone())
    }

    pub fn identd(&self) -> Option<Arc<Identd>> {
        self.identd.get().cloned()
    }

    pub fn set_identd(&self, identd: Arc<Identd>) {
        let _ = self.identd.set(identd);
    }

    pub fn web_push(&self) -> Option<Arc<webpush::Sender>> {
        self.web_push.get().cloned()
    }

    pub fn next_downstream_id(&self) -> u64 {
        self.next_downstream_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Loads users and starts background tasks.
    pub fn start(self: &Arc<Self>) -> Result<()> {
        self.load_web_push_config()?;

        let users = self.db.list_users()?;
        {
            let mut map = self.users.lock().unwrap();
            for user in users {
                self.add_user_locked(&mut map, user);
            }
        }

        let srv = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(4 * 3600));
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = srv.stop.cancelled() => return,
                }
                if let Err(e) = srv.disable_inactive_users().await {
                    logf!(srv.logger, "failed to disable inactive users: {e}");
                }
            }
        });

        // Expired messages are deleted daily, the first time on startup,
        // see startup_maintenance
        let srv = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(24 * 3600));
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = srv.stop.cancelled() => return,
                }
                srv.run_message_expiry(false).await;
            }
        });
        Ok(())
    }

    fn load_web_push_config(&self) -> Result<()> {
        let configs = self
            .db
            .list_web_push_configs()
            .map_err(|e| anyhow!("failed to list Web push configs: {e}"))?;
        let config = match configs.len() {
            0 => {
                logf!(self.logger, "generating Web push VAPID key pair");
                let (private, public) = webpush::generate_vapid_keys();
                let mut config = db::WebPushConfig {
                    vapid_public: public,
                    vapid_private: private,
                    ..Default::default()
                };
                self.db
                    .store_web_push_config(&mut config)
                    .map_err(|e| anyhow!("failed to store Web push config: {e}"))?;
                config
            }
            1 => configs.into_iter().next().unwrap(),
            n => bail!("expected zero or one Web push config, got {n}"),
        };
        let _ = self.web_push.set(Arc::new(webpush::Sender::new(
            &config.vapid_public,
            &config.vapid_private,
        )));
        Ok(())
    }

    /// Stops all users and waits for them to finish.
    pub async fn shutdown(&self) {
        logf!(self.logger, "shutting down server");
        self.stop.cancel();
        self.shutdown.store(true, Ordering::SeqCst);
        let handles: Vec<UserHandle> = self.users.lock().unwrap().values().cloned().collect();
        for h in &handles {
            h.send(Event::Stop);
        }
        logf!(self.logger, "waiting for users to finish");
        for h in handles {
            h.done.cancelled().await;
        }
    }

    pub fn stopped(&self) -> CancellationToken {
        self.stop.clone()
    }

    fn add_user_locked(
        self: &Arc<Self>,
        map: &mut HashMap<String, UserHandle>,
        record: db::User,
    ) -> UserHandle {
        if let Some(running) = map.get(&record.username) {
            // e.g. created on authentication while start() listed users
            return running.clone();
        }
        logf!(
            self.logger,
            "starting bouncer for user {:?}",
            record.username
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = UserHandle {
            tx: tx.clone(),
            done: CancellationToken::new(),
            num_downstream_conns: Arc::new(AtomicI64::new(0)),
            record: Arc::new(Mutex::new(record.clone())),
        };
        let username = record.username.clone();
        map.insert(username.clone(), handle.clone());

        let user = User::new(self.clone(), record, tx, &handle);
        let srv = self.clone();
        let done = handle.done.clone();
        let task = tokio::spawn(user.run(rx));
        tokio::spawn(async move {
            if let Err(e) = task.await
                && e.is_panic()
            {
                logf!(srv.logger, "panic serving user {username:?}: {e}");
                crate::metrics::inc(&srv.metrics.worker_panics_total);
            }
            srv.users.lock().unwrap().remove(&username);
            done.cancel();
        });
        handle
    }

    /// Creates a user in the database and starts it.
    pub fn create_user(self: &Arc<Self>, mut record: db::User) -> Result<UserHandle> {
        let _creating = self.user_creation.lock().unwrap();
        if self.is_shutting_down() {
            bail!("server is shutting down");
        }
        if self.users.lock().unwrap().contains_key(&record.username) {
            bail!("user {:?} already exists", record.username);
        }
        self.db
            .store_user(&mut record)
            .map_err(|e| anyhow!("could not create user in db: {e}"))?;
        let mut users = self.users.lock().unwrap();
        // The database write may have overlapped shutdown. Hold the same lock
        // used by its snapshot: either this handle is included or none starts.
        if self.is_shutting_down() {
            bail!("server is shutting down");
        }
        Ok(self.add_user_locked(&mut users, record))
    }

    pub fn get_user(&self, name: &str) -> Option<UserHandle> {
        self.users.lock().unwrap().get(name).cloned()
    }

    pub fn for_each_user(&self, mut f: impl FnMut(&UserHandle)) {
        let handles: Vec<UserHandle> = self.users.lock().unwrap().values().cloned().collect();
        for h in &handles {
            f(h);
        }
    }

    /// Copies of the records of all running users.
    pub fn user_records(&self) -> Vec<db::User> {
        self.users
            .lock()
            .unwrap()
            .values()
            .map(|h| h.record())
            .collect()
    }

    pub async fn get_or_create_user(self: &Arc<Self>, username: &str) -> Result<UserHandle> {
        if let Some(u) = self.get_user(username) {
            return Ok(u);
        }
        let name = username.to_owned();
        if db::run(&self.db, move |db| db.get_user(&name))
            .await?
            .is_some()
        {
            // Another connection may have just created it
            let srv = self.clone();
            let name = username.to_owned();
            let loaded = tokio::task::spawn_blocking(move || {
                // A concurrent creator may have committed its DB row while
                // still registering the running handle.
                let _creating = srv.user_creation.lock().unwrap();
                srv.get_user(&name)
            })
            .await?;
            if let Some(u) = loaded {
                return Ok(u);
            }
            bail!(
                "user {username:?} exists in the DB but hasn't been loaded by the bouncer -- a \
                    restart may help"
            );
        }
        if !self.config().basic.enable_users_on_auth {
            bail!("cannot find user {username:?} in the DB");
        }
        // Can't find the user in the DB -- try to create it
        let record = db::User::new(username);
        let srv = self.clone();
        let res = tokio::task::spawn_blocking(move || srv.create_user(record))
            .await
            .map_err(|e| anyhow!("database task failed: {e}"))?;
        match res {
            Ok(u) => Ok(u),
            // Another connection created it meanwhile
            Err(_) if let Some(u) = self.get_user(username) => Ok(u),
            Err(e) => Err(anyhow!(
                "failed to automatically create user {username:?} after successful authentication: \
                    {e}"
            )),
        }
    }

    pub fn stats(&self) -> ServerStats {
        ServerStats {
            users: self.users.lock().unwrap().len(),
            downstreams: self.metrics.downstreams.load(Ordering::Relaxed),
            upstreams: self.metrics.upstreams.load(Ordering::Relaxed),
        }
    }

    pub fn render_metrics(&self) -> String {
        self.metrics.render(self.users.lock().unwrap().len())
    }
}

#[cfg(test)]
mod tests;
