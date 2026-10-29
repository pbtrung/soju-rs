//! The bouncer server: users, configuration, admin connections and
//! background tasks (port of upstream's server.go).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::auth::Authenticator;
use crate::config::{self, MsgStoreDriver};
use crate::conn::{Conn, ConnReader};
use crate::database::{self as db, Database};
use crate::identd::Identd;
use crate::irc::{Prefix, numerics::*};
use crate::logger::Logger;
use crate::metrics::Metrics;
use crate::user::{Event, User, UserHandle};
use crate::webpush;
use crate::{logf, msg, xirc};

pub const DOWNSTREAM_REGISTER_TIMEOUT: Duration = Duration::from_secs(30);

/// Message expiry vacuums the database when this fraction of it was free
/// before deleting messages. With steady traffic, the space freed each day
/// is reused the next day: vacuuming would only shrink the file for it to
/// grow back, and rewriting the whole database blocks all users meanwhile.
/// Free space only builds up when less is stored, e.g. after shortening
/// message-expiry.
const VACUUM_FREE_RATIO: f64 = 0.25;

/// Message expiry deletes this many messages at a time, pausing for
/// EXPIRY_CHUNK_PAUSE in between, so that users waiting on the database
/// aren't blocked for the whole deletion.
const EXPIRY_CHUNK_SIZE: usize = 2000;
const EXPIRY_CHUNK_PAUSE: Duration = Duration::from_millis(10);

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
    /// the setting in effect is kept until restart.
    pub fn reload_config(&self, mut cfg: ServerConfig) {
        let cur = self.config();
        if cfg.basic.msg_store != cur.basic.msg_store {
            logf!(
                self.logger,
                "message-store changed: restart to apply the new setting"
            );
            cfg.basic.msg_store = cur.basic.msg_store.clone();
        }
        self.set_config(cfg);
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

        // The first tick completes immediately: expired messages are deleted
        // on startup, then daily. Only local SQLite is vacuumed on startup.
        let srv = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(24 * 3600));
            let mut startup = true;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = srv.stop.cancelled() => return,
                }
                // Deleting and vacuuming can take a while: keep it off the
                // async runtime's workers
                let s = srv.clone();
                let force_vacuum = std::mem::replace(&mut startup, false);
                match tokio::task::spawn_blocking(move || s.delete_expired_messages(force_vacuum))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => logf!(srv.logger, "failed to delete expired messages: {e}"),
                    Err(e) => logf!(srv.logger, "failed to delete expired messages: {e}"),
                }
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
        if self.users.lock().unwrap().contains_key(&record.username) {
            bail!("user {:?} already exists", record.username);
        }
        self.db
            .store_user(&mut record)
            .map_err(|e| anyhow!("could not create user in db: {e}"))?;
        Ok(self.add_user_locked(&mut self.users.lock().unwrap(), record))
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
                // still provisioning the tenant or registering the handle.
                let _creating = srv.user_creation.lock().unwrap();
                srv.get_user(&name)
            })
            .await?;
            if let Some(u) = loaded {
                return Ok(u);
            }
            bail!(
                "user {username:?} exists in the DB but hasn't been loaded by the bouncer -- a restart may help"
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
                "failed to automatically create user {username:?} after successful authentication: {e}"
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
        let mut metrics = self.metrics.render(self.users.lock().unwrap().len());
        if let Some((read, written)) = self.db.observed_turso_usage() {
            use std::fmt::Write;
            for (name, value, help) in [
                (
                    "soju_turso_rows_read_total",
                    read,
                    "Rows read reported by received Turso SQL responses",
                ),
                (
                    "soju_turso_rows_written_total",
                    written,
                    "Rows written reported by received Turso SQL responses",
                ),
            ] {
                let _ = writeln!(
                    metrics,
                    "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
                );
            }
        }
        metrics
    }

    async fn disable_inactive_users(&self) -> Result<()> {
        let delay = self.config().basic.disable_inactive_users_delay;
        if delay.is_zero() {
            return Ok(());
        }
        let limit = Utc::now() - chrono::Duration::from_std(delay)?;
        let usernames = db::run(&self.db, move |db| db.list_inactive_usernames(limit))
            .await
            .map_err(|e| anyhow!("failed to list inactive users: {e}"))?;
        if usernames.is_empty() {
            return Ok(());
        }

        // Filter out users with active downstream connections
        let users: Vec<(String, UserHandle)> = {
            let map = self.users.lock().unwrap();
            usernames
                .into_iter()
                .filter_map(|name| {
                    // TODO: disable the user in the DB if it isn't running
                    let h = map.get(&name)?;
                    if h.num_downstream_conns.load(Ordering::SeqCst) > 0 {
                        return None;
                    }
                    Some((name, h.clone()))
                })
                .collect()
        };
        if users.is_empty() {
            return Ok(());
        }

        logf!(self.logger, "found {} inactive users", users.len());
        for (name, h) in users {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let sent = h.send(Event::UserUpdate {
                password: None,
                admin: None,
                enabled: Some(false),
                max_networks: None,
                done: tx,
            });
            if !sent {
                // The user stopped meanwhile, e.g. it was deleted
                continue;
            }
            match tokio::time::timeout(Duration::from_secs(60), rx).await {
                Ok(Ok(Ok(()))) => logf!(self.logger, "disabled inactive user {name:?}"),
                Ok(Ok(Err(e))) => bail!(e),
                _ => bail!("context deadline exceeded"),
            }
        }
        Ok(())
    }

    /// Deletes expired messages. Local SQLite is vacuumed if force_vacuum is
    /// set, or enough pages are free. Turso never receives file maintenance SQL.
    pub(crate) fn delete_expired_messages(&self, force_vacuum: bool) -> Result<()> {
        let cfg = self.config();
        let expiry = cfg.basic.message_expiry;
        if expiry.is_zero() {
            return Ok(());
        }
        let before = Utc::now() - chrono::Duration::from_std(expiry)?;
        let before_str = xirc::format_server_time(before);
        logf!(
            self.logger,
            "soju-rs v{}: running message expiry",
            crate::VERSION
        );
        match cfg.basic.msg_store.driver {
            MsgStoreDriver::Db | MsgStoreDriver::Turso => {
                let local_sqlite = cfg.basic.msg_store.driver == MsgStoreDriver::Db;
                // Measured before deleting: the space freed by the previous
                // run has normally been reused since, what's still free is
                // wasted
                let free = if local_sqlite {
                    self.db
                        .free_page_ratio()
                        .map_err(|e| anyhow!("failed to get db free space: {e}"))?
                } else {
                    0.0
                };
                let mut n = 0;
                loop {
                    let deleted = self
                        .db
                        .delete_messages_before(before, EXPIRY_CHUNK_SIZE)
                        .map_err(|e| anyhow!("failed to delete expired messages from db: {e}"))?;
                    n += deleted;
                    if deleted < EXPIRY_CHUNK_SIZE || self.stop.is_cancelled() {
                        break;
                    }
                    // Runs on a blocking thread, see start
                    std::thread::sleep(EXPIRY_CHUNK_PAUSE);
                }
                logf!(
                    self.logger,
                    "deleted {n} expired messages older than {before_str} from db"
                );
                if n > 0 {
                    let start = Instant::now();
                    self.db
                        .optimize()
                        .map_err(|e| anyhow!("failed to optimize db: {e}"))?;
                    logf!(
                        self.logger,
                        "optimized db in {:.1}s",
                        start.elapsed().as_secs_f64()
                    );
                }
                if local_sqlite && (force_vacuum || free >= VACUUM_FREE_RATIO) {
                    let start = Instant::now();
                    self.db
                        .vacuum()
                        .map_err(|e| anyhow!("failed to vacuum db: {e}"))?;
                    logf!(
                        self.logger,
                        "vacuumed db in {:.1}s ({:.0}% of it was free)",
                        start.elapsed().as_secs_f64(),
                        free * 100.0
                    );
                }
                if local_sqlite {
                    self.db
                        .shrink_memory()
                        .map_err(|e| anyhow!("failed to shrink db memory: {e}"))?;
                }
            }
            MsgStoreDriver::Memory => {}
        }
        Ok(())
    }

    /// Serves an administrative connection (sojuctl).
    pub async fn serve_admin(self: Arc<Self>, conn: Conn, mut reader: ConnReader, logger: Logger) {
        let prefix = self.prefix();
        if self.is_shutting_down() {
            conn.send_message(msg!("ERROR", "Server is shutting down"));
            conn.shutdown();
            return;
        }
        loop {
            let msg = match reader.read_message().await {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    logf!(logger, "failed to read IRC command: {e}");
                    break;
                }
            };
            match msg.command.as_str() {
                "CAP" | "NICK" | "USER" | "PASS" => {
                    // Ensure regular IRC clients cannot connect. This is
                    // important to e.g. prevent unprivileged soju users from
                    // connecting to the admin socket.
                    conn.send_message(msg!("ERROR", "This is not a regular IRC server"));
                    break;
                }
                "BOUNCERSERV" => {
                    if msg.params.is_empty() {
                        conn.send_message(msg!(
                            ERR_NEEDMOREPARAMS,
                            "*",
                            msg.command.clone(),
                            "Not enough parameters"
                        ));
                        continue;
                    }
                    let text = msg.params[0].clone();
                    let srv = self.clone();
                    let (res, prints, deferred) = tokio::task::spawn_blocking(move || {
                        let rt = tokio::runtime::Handle::current();
                        let _guard = rt.enter();
                        let mut ctx = crate::service::ServiceContext::for_user(srv, true, false);
                        let res = crate::service::handle_global_service_command(&mut ctx, &text);
                        (res, ctx.take_prints(), ctx.take_deferred())
                    })
                    .await
                    .unwrap_or_else(|e| (Err(anyhow!("internal error: {e}")), Vec::new(), None));
                    for line in prints {
                        conn.send_message(msg!("PRIVMSG", "*", line).with_prefix(prefix.clone()));
                    }
                    let res = match (res, deferred) {
                        (Ok(()), Some(deferred)) => {
                            let (conn, prefix) = (conn.clone(), prefix.clone());
                            let print = Box::new(move |line| {
                                conn.send_message(
                                    msg!("PRIVMSG", "*", line).with_prefix(prefix.clone()),
                                );
                            });
                            deferred(print).await
                        }
                        (res, _) => res,
                    };
                    match res {
                        Err(e) => conn.send_message(
                            msg!("FAIL", msg.command.clone(), e.to_string())
                                .with_prefix(prefix.clone()),
                        ),
                        Ok(()) => conn.send_message(
                            msg!(msg.command.clone(), "OK").with_prefix(prefix.clone()),
                        ),
                    }
                }
                _ => conn.send_message(
                    msg!(
                        ERR_UNKNOWNCOMMAND,
                        "*",
                        msg.command.clone(),
                        "Unknown command"
                    )
                    .with_prefix(prefix.clone()),
                ),
            }
        }
        conn.shutdown();
    }
}

/// Builds the run-time configuration from the config file.
pub async fn load_server_config(raw: &config::Server) -> Result<ServerConfig> {
    // A hostname without a dot can confuse clients
    if !raw.basic.hostname.contains('.') {
        eprintln!(
            "{} warning: hostname {:?} is not a fully qualified domain name",
            chrono::Local::now().format("%Y/%m/%d %H:%M:%S"),
            raw.basic.hostname
        );
    }

    let mut motd = String::new();
    if !raw.motd_path.is_empty() {
        motd = std::fs::read_to_string(&raw.motd_path)
            .map_err(|e| anyhow!("failed to load MOTD: {e}"))?;
        if let Some(m) = motd.strip_suffix('\n') {
            motd = m.to_owned();
        }
    }

    let (mut icon_url, mut icon_path) = (String::new(), String::new());
    if raw.icon.contains("://") {
        let u = db::parse_url(&raw.icon).map_err(|e| anyhow!("invalid icon URL: {e}"))?;
        if u.scheme != "https" {
            bail!("unsupported icon URL scheme: {}", u.scheme);
        }
        icon_url = raw.icon.clone();
    } else if !raw.icon.is_empty() {
        std::fs::metadata(&raw.icon).map_err(|e| anyhow!("failed to load icon: {e}"))?;
        icon_path = raw.icon.clone();
    }

    let auth = Authenticator::from_config(&raw.auth).await?;

    Ok(ServerConfig {
        basic: raw.basic.clone(),
        icon_path,
        icon_url,
        motd,
        auth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn create_user_on_auth_concurrently() {
        let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
        let mut cfg = ServerConfig::default();
        cfg.basic.enable_users_on_auth = true;
        srv.set_config(cfg);
        srv.start().unwrap();
        // e.g. a client connecting to several networks on first login
        let results =
            futures_util::future::join_all((0..8).map(|_| srv.get_or_create_user("alice"))).await;
        for res in results {
            res.unwrap();
        }
        assert_eq!(srv.db.list_users().unwrap().len(), 1);
        srv.shutdown().await;
    }

    #[test]
    fn msg_store_not_reloaded() {
        let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
        srv.set_config(ServerConfig::default());
        let mut cfg = ServerConfig::default();
        cfg.basic.msg_store.driver = MsgStoreDriver::Memory;
        cfg.basic.title = "reloaded".to_owned();
        srv.reload_config(cfg);
        let cfg = srv.config();
        assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Db);
        assert_eq!(cfg.basic.title, "reloaded");
    }
}
