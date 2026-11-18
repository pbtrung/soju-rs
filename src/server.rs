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

/// Runs one owned actor job off async workers, with at most 16 admitted jobs
/// across the process. Callers await completion before handling another event.
/// Started work runs to completion even if its awaiting future is cancelled.
///
/// # Errors
/// Returns an error if runtime shutdown cancels a job before it starts.
///
/// # Panics
/// Resumes a panic from the actor job so existing worker supervision records it.
pub async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R> {
    static ACTOR_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);
    let permit = ACTOR_WORKERS.acquire().await?;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    }).await {
        Ok(value) => Ok(value),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(anyhow!(error).context("actor job cancelled before completion")),
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
        "localhost".clone_into(&mut basic.hostname);
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
    maintenance_tasks: tokio_util::task::TaskTracker,
    expiry_state: Mutex<maintenance::ExpiryState>,
    pub(crate) accepted_sessions: Arc<crate::listen::SessionBudget>,
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
            maintenance_tasks: tokio_util::task::TaskTracker::new(),
            expiry_state: Mutex::default(),
            accepted_sessions: Arc::default(),
            client_cert_auth: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn config(&self) -> Arc<ServerConfig> {
        crate::state_lock::read(&self.config).clone()
    }

    pub fn set_config(&self, cfg: ServerConfig) {
        self.client_cert_auth
            .store(cfg.basic.client_cert_auth, Ordering::Relaxed);
        *crate::state_lock::write(&self.config) = Arc::new(cfg);
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
            self.schedule_message_expiry(true);
        }
    }

    pub fn prefix(&self) -> Prefix {
        Prefix::new(self.config().basic.hostname.clone())
    }

    pub fn identd(&self) -> Option<Arc<Identd>> {
        self.identd.get().cloned()
    }

    pub fn set_identd(&self, identd: Arc<Identd>) {
        // Installation is first-wins, including concurrent calls.
        self.identd.get_or_init(|| identd);
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
    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
    pub fn start(self: &Arc<Self>) -> Result<()> {
        self.load_web_push_config()?;

        let users = self.db.list_users()?;
        {
            let mut map = crate::state_lock::lock(&self.users);
            for user in users {
                self.add_user_locked(&mut map, user);
            }
        }

        let srv = self.clone();
        self.maintenance_tasks.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_hours(4));
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    () = srv.stop.cancelled() => return,
                }
                if let Err(e) = srv.disable_inactive_users().await {
                    logf!(srv.logger, "failed to disable inactive users: {e}");
                }
            }
        });

        // Expired messages are deleted daily, the first time on startup,
        // see startup_maintenance
        let srv = self.clone();
        self.maintenance_tasks.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_hours(24));
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    () = srv.stop.cancelled() => return,
                }
                srv.schedule_message_expiry(false);
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
            1 => configs.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("required user or connection state is unavailable")
            })?,
            n => bail!("expected zero or one Web push config, got {n}"),
        };
        let sender = Arc::new(webpush::Sender::new(
            &config.vapid_public,
            &config.vapid_private,
        )?);
        self.web_push.get_or_init(|| sender);
        Ok(())
    }

    /// Stops all users and waits for them to finish.
    pub async fn shutdown(&self) {
        logf!(self.logger, "shutting down server");
        self.shutdown.store(true, Ordering::SeqCst);
        self.stop.cancel();
        {
            // Scheduling registers tracked work while holding this same lock.
            // Once closed, no later reload can enqueue maintenance.
            let mut expiry = crate::state_lock::lock(&self.expiry_state);
            expiry.requested = None;
            self.maintenance_tasks.close();
        }
        let handles: Vec<UserHandle> = crate::state_lock::lock(&self.users)
            .values()
            .cloned()
            .collect();
        for h in &handles {
            h.notify(Event::Stop);
        }
        logf!(self.logger, "waiting for users to finish");
        for h in handles {
            h.done.cancelled().await;
        }
        self.maintenance_tasks.wait().await;
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
            crate::state_lock::lock(&srv.users).remove(&username);
            done.cancel();
        });
        handle
    }

    /// Creates a user in the database and starts it.
    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
    pub fn create_user(self: &Arc<Self>, mut record: db::User) -> Result<UserHandle> {
        let _creating = crate::state_lock::lock(&self.user_creation);
        if self.is_shutting_down() {
            bail!("server is shutting down");
        }
        if crate::state_lock::lock(&self.users).contains_key(&record.username) {
            bail!("user {:?} already exists", record.username);
        }
        self.db
            .store_user(&mut record)
            .map_err(|e| anyhow!("could not create user in db: {e}"))?;
        let mut users = crate::state_lock::lock(&self.users);
        // The database write may have overlapped shutdown. Hold the same lock
        // used by its snapshot: either this handle is included or none starts.
        if self.is_shutting_down() {
            bail!("server is shutting down");
        }
        Ok(self.add_user_locked(&mut users, record))
    }

    pub fn get_user(&self, name: &str) -> Option<UserHandle> {
        crate::state_lock::lock(&self.users).get(name).cloned()
    }

    pub fn for_each_user(&self, mut f: impl FnMut(&UserHandle)) {
        let handles: Vec<UserHandle> = crate::state_lock::lock(&self.users)
            .values()
            .cloned()
            .collect();
        for h in &handles {
            f(h);
        }
    }

    /// Copies of the records of all running users.
    pub fn user_records(&self) -> Vec<db::User> {
        crate::state_lock::lock(&self.users)
            .values()
            .map(super::user::UserHandle::record)
            .collect()
    }

    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
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
                let _creating = crate::state_lock::lock(&srv.user_creation);
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
            users: crate::state_lock::lock(&self.users).len(),
            downstreams: self.metrics.downstreams.load(Ordering::Relaxed),
            upstreams: self.metrics.upstreams.load(Ordering::Relaxed),
        }
    }

    /// Physical accepted streams still alive, including incomplete handshakes
    /// and upgraded `WebSockets`. This excludes outbound upstream connections.
    #[must_use]
    pub fn accepted_sessions(&self) -> usize {
        self.accepted_sessions.active()
    }

    /// Fixed maintenance timers and any coalesced expiry worker still running.
    #[must_use]
    pub fn maintenance_tasks(&self) -> usize { self.maintenance_tasks.len() }

    pub fn render_metrics(&self) -> String {
        format!(
            "{}# HELP soju_accepted_sessions Physical accepted sessions including handshakes and upgrades.\n# TYPE soju_accepted_sessions gauge\nsoju_accepted_sessions {}\n",
            self.metrics
                .render(crate::state_lock::lock(&self.users).len()),
            self.accepted_sessions(),
        )
    }
}

#[cfg(test)]
mod tests;
