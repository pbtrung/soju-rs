//! BouncerServ, the IRC service used to manage the bouncer (port of
//! upstream's service.go).

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::downstream::DownstreamConn;
use crate::flags::{FlagError, FlagSet, Parsed};
use crate::ircutil::split_words;
use crate::logf;
use crate::server::Server;
use crate::user::User;

mod auth;
mod channel;
mod duration;
mod network;
mod server;
mod user;

use auth::*;
use channel::*;
pub use duration::*;
use network::*;
use server::*;
use user::*;

/// Where a service command runs from, and how its output is collected.
pub struct ServiceContext {
    pub srv: Arc<Server>,
    /// The caller's nickname, if any
    pub nick: String,
    /// The network the caller is bound to, if any
    pub network_id: Option<i64>,
    pub peer_cert: Option<Vec<u8>>,
    pub remote_ip: Option<IpAddr>,
    pub admin: bool,
    /// When waiting on other users gives up, if ever
    deadline: Option<tokio::time::Instant>,
    prints: Vec<String>,
    deferred: Option<Deferred>,
}

/// Prints a line of a deferred command's output.
pub type Print = Box<dyn FnMut(String) + Send>;

/// The rest of a command that waits on other users' event loops, or runs
/// CPU-heavy work. The event loop running the command can't block on them:
/// with a single-threaded runtime, nothing else would run meanwhile. The
/// caller spawns it instead, once the command returned.
pub type Deferred =
    Box<dyn FnOnce(Print) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send>;

impl ServiceContext {
    pub fn for_downstream(srv: Arc<Server>, admin: bool, dc: &DownstreamConn) -> ServiceContext {
        ServiceContext {
            srv,
            nick: dc.nick.clone(),
            network_id: dc.network_id,
            peer_cert: dc.info.peer_cert.clone(),
            remote_ip: dc.info.remote_ip,
            admin,
            deadline: Some(tokio::time::Instant::now() + HANDLE_DOWNSTREAM_MESSAGE_TIMEOUT),
            prints: Vec::new(),
            deferred: None,
        }
    }

    /// A context for commands run without a downstream: from the admin
    /// socket, or from another user's event loop (user run). The latter
    /// must not wait forever on other users, see wait.
    pub fn for_user(srv: Arc<Server>, admin: bool, in_event_loop: bool) -> ServiceContext {
        ServiceContext {
            srv,
            nick: String::new(),
            network_id: None,
            peer_cert: None,
            remote_ip: None,
            admin,
            deadline: in_event_loop
                .then(|| tokio::time::Instant::now() + HANDLE_DOWNSTREAM_MESSAGE_TIMEOUT),
            prints: Vec::new(),
            deferred: None,
        }
    }

    pub fn print(&mut self, s: impl Into<String>) {
        self.prints.push(s.into());
    }

    pub fn take_prints(&mut self) -> Vec<String> {
        std::mem::take(&mut self.prints)
    }

    /// Takes the rest of the command, if it waits on other users.
    pub fn take_deferred(&mut self) -> Option<Deferred> {
        self.deferred.take()
    }

    /// Continues the command with f, once the caller's event loop is free.
    fn defer<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Print) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.deferred = Some(Box::new(move |print| Box::pin(f(print))));
    }
}

/// Waits on a future, e.g. a reply from another user's event loop. Returns
/// None if the deadline is reached first.
async fn wait<F: Future>(deadline: Option<tokio::time::Instant>, fut: F) -> Option<F::Output> {
    match deadline {
        Some(d) => tokio::time::timeout_at(d, fut).await.ok(),
        None => Some(fut.await),
    }
}

/// Runs CPU-heavy work (key generation, password hashing) on a blocking
/// thread, from a deferred command.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    crate::util::cpu_work(f).await?
}

/// Hashes a password on a blocking thread, from a deferred command.
async fn hash_password(password: String) -> Result<String> {
    blocking(move || {
        let mut record = crate::database::User::new("");
        record.set_password(&password)?;
        Ok(record.password)
    })
    .await
}

/// Runs f on a user's event loop and returns its result, from a deferred
/// command.
async fn run_on_user<T: Send + 'static>(
    tx: &tokio::sync::mpsc::UnboundedSender<crate::user::Event>,
    f: impl FnOnce(&mut User) -> Result<T> + Send + 'static,
) -> Result<T> {
    let (done, rx) = tokio::sync::oneshot::channel();
    let ev = crate::user::Event::Run(Box::new(move |u| {
        let _ = done.send(f(u));
    }));
    if tx.send(ev).is_err() {
        bail!("user stopped");
    }
    rx.await.map_err(|_| anyhow!("user stopped"))?
}

/// How long a downstream's command may wait on other users, like upstream's
/// handleDownstreamMessageTimeout.
const HANDLE_DOWNSTREAM_MESSAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

type Handler = fn(Option<&mut User>, &mut ServiceContext, &[String]) -> Result<()>;

#[derive(Debug)]
struct Command {
    name: &'static str,
    usage: &'static str,
    desc: &'static str,
    handle: Option<Handler>,
    children: Vec<Command>,
    admin: bool,
    global: bool,
}

impl Command {
    const fn leaf(
        name: &'static str,
        usage: &'static str,
        desc: &'static str,
        handle: Handler,
    ) -> Command {
        Command {
            name,
            usage,
            desc,
            handle: Some(handle),
            children: Vec::new(),
            admin: false,
            global: false,
        }
    }

    fn group(name: &'static str, children: Vec<Command>) -> Command {
        Command {
            name,
            usage: "",
            desc: "",
            handle: None,
            children,
            admin: false,
            global: false,
        }
    }

    const fn admin(mut self) -> Command {
        self.admin = true;
        self
    }

    const fn global(mut self) -> Command {
        self.global = true;
        self
    }
}

fn commands() -> &'static [Command] {
    use std::sync::OnceLock;
    static COMMANDS: OnceLock<Vec<Command>> = OnceLock::new();
    COMMANDS.get_or_init(|| {
        vec![
            Command::leaf("help", "[command]", "print help message", handle_help).global(),
            Command::group(
                "network",
                vec![
                    Command::leaf(
                        "create",
                        "-addr <addr> [-name name] [-username username] [-pass pass] [-realname realname] [-certfp fingerprint] [-nick nick] [-auto-away auto-away] [-enabled enabled] [-ignore-limit] [-connect-command command]...",
                        "add a new network",
                        handle_network_create,
                    ),
                    Command::leaf(
                        "status",
                        "",
                        "show a list of saved networks and their current status",
                        handle_network_status,
                    ),
                    Command::leaf("info", "[name]", "show the configuration of a network", handle_network_info),
                    Command::leaf(
                        "update",
                        "[name] [-addr addr] [-name name] [-username username] [-pass pass] [-realname realname] [-certfp fingerprint] [-nick nick] [-auto-away auto-away] [-enabled enabled] [-ignore-limit] [-connect-command command]...",
                        "update a network",
                        handle_network_update,
                    ),
                    Command::leaf("delete", "<name>", "delete a network", handle_network_delete),
                    Command::leaf("quote", "[name] <command>", "send a raw line to a network", handle_network_quote),
                ],
            ),
            Command::group(
                "certfp",
                vec![
                    Command::leaf(
                        "generate",
                        "[-key-type rsa|ecdsa|ed25519] [-bits N] [-network name]",
                        "generate a new self-signed certificate, defaults to using RSA-3072 key",
                        handle_certfp_generate,
                    ),
                    Command::leaf(
                        "fingerprint",
                        "[-network name]",
                        "show fingerprints of certificate",
                        handle_certfp_fingerprints,
                    ),
                ],
            ),
            Command::group(
                "sasl",
                vec![
                    Command::leaf("status", "[-network name]", "show SASL status", handle_sasl_status),
                    Command::leaf(
                        "set-plain",
                        "[-network name] <username> <password>",
                        "set SASL PLAIN credentials",
                        handle_sasl_set_plain,
                    ),
                    Command::leaf(
                        "reset",
                        "[-network name]",
                        "disable SASL authentication and remove stored credentials",
                        handle_sasl_reset,
                    ),
                ],
            ),
            Command::group(
                "user",
                vec![
                    Command::leaf("status", "[username]", "show a list of users and their current status", handle_user_status)
                        .admin()
                        .global(),
                    Command::leaf(
                        "create",
                        "-username <username> -password <password> [-disable-password] [-admin] [-nick <nick>] [-realname <realname>] [-enabled=false] [-max-networks <max-networks>]",
                        "create a new soju user",
                        handle_user_create,
                    )
                    .admin()
                    .global(),
                    Command::leaf(
                        "update",
                        "[username] [-password <password>] [-disable-password] [-admin true|false] [-nick <nick>] [-realname <realname>] [-enabled true|false] [-max-networks <max-networks>]",
                        "update a user",
                        handle_user_update,
                    )
                    .global(),
                    Command::leaf("delete", "<username> [confirmation token]", "delete a user", handle_user_delete).global(),
                    Command::leaf("run", "<username> <command>", "run a command as another user", handle_user_run)
                        .admin()
                        .global(),
                ],
            )
            .global(),
            Command::group(
                "channel",
                vec![
                    Command::leaf(
                        "status",
                        "[-network name]",
                        "show a list of saved channels and their current status",
                        handle_channel_status,
                    ),
                    Command::leaf(
                        "create",
                        "<name> [-detached true|false] [-relay-detached default|none|highlight|message] [-reattach-on default|none|highlight|message] [-detach-after <duration>] [-detach-on default|none|highlight|message]",
                        "create a channel",
                        handle_channel_create,
                    ),
                    Command::leaf(
                        "update",
                        "<name> [-detached true|false] [-relay-detached default|none|highlight|message] [-reattach-on default|none|highlight|message] [-detach-after <duration>] [-detach-on default|none|highlight|message]",
                        "update a channel",
                        handle_channel_update,
                    ),
                    Command::leaf("delete", "<name>", "delete a channel", handle_channel_delete),
                ],
            ),
            Command::group(
                "device-certificate",
                vec![
                    Command::leaf("status", "", "show a list of device certificates", handle_device_cert_status),
                    Command::leaf(
                        "create",
                        "-fingerprint <fingerprint> -label <label>",
                        "create a new device certificate",
                        handle_device_cert_create,
                    ),
                    Command::leaf("delete", "<fingerprint>", "delete a device certificate", handle_device_cert_delete),
                ],
            ),
            Command::group(
                "server",
                vec![
                    Command::leaf("status", "", "show server statistics", handle_server_status)
                        .admin()
                        .global(),
                    Command::leaf(
                        "notice",
                        "<notice>",
                        "broadcast a notice to all connected bouncer users",
                        handle_server_notice,
                    )
                    .admin()
                    .global(),
                    Command::leaf(
                        "debug",
                        "true|false",
                        "enable/disable debug logging to stderr (will leak sensitive information)",
                        handle_server_debug,
                    )
                    .admin()
                    .global(),
                ],
            )
            .admin()
            .global(),
        ]
    })
}

fn find<'a>(cmds: &'a [Command], name: &str) -> Result<&'a Command> {
    if let Some(c) = cmds.iter().find(|c| c.name == name) {
        return Ok(c);
    }
    let mut found: Option<&Command> = None;
    for c in cmds {
        if !c.name.starts_with(name) {
            continue;
        }
        if found.is_some() {
            bail!("command {name:?} is ambiguous");
        }
        found = Some(c);
    }
    found.ok_or_else(|| anyhow!("command {name:?} not found"))
}

/// Looks up a command, returning it and the remaining parameters.
fn get<'a, 'b>(cmds: &'a [Command], params: &'b [String]) -> Result<(&'a Command, &'b [String])> {
    let Some((name, rest)) = params.split_first() else {
        bail!("no command specified");
    };
    let cmd = find(cmds, name)?;
    if rest.is_empty() || cmd.children.is_empty() {
        return Ok((cmd, rest));
    }
    get(&cmd.children, rest)
}

fn sorted(cmds: &[Command]) -> Vec<&Command> {
    let mut v: Vec<&Command> = cmds.iter().collect();
    v.sort_by_key(|c| c.name);
    v
}

fn append_help(
    cmds: &[Command],
    prefix: &[String],
    admin: bool,
    global: bool,
    l: &mut Vec<String>,
) {
    for cmd in sorted(cmds) {
        if cmd.admin && !admin {
            continue;
        }
        if !cmd.global && global {
            continue;
        }
        let mut words = prefix.to_vec();
        words.push(cmd.name.to_owned());
        if cmd.children.is_empty() {
            l.push(words.join(" "));
        } else {
            append_help(&cmd.children, &words, admin, global, l);
        }
    }
}

pub fn handle_service_privmsg(u: &mut User, ctx: &mut ServiceContext, text: &str) -> Result<()> {
    let words = split_words(text).map_err(|e| anyhow!("failed to parse command: {e}"))?;
    handle_service_command(u, ctx, &words)
}

pub fn handle_service_command(
    u: &mut User,
    ctx: &mut ServiceContext,
    words: &[String],
) -> Result<()> {
    run_command(Some(u), ctx, words)
}

/// Runs a command without a user (from the admin socket).
pub fn handle_global_service_command(ctx: &mut ServiceContext, text: &str) -> Result<()> {
    let words = split_words(text).map_err(|e| anyhow!("failed to parse command: {e}"))?;
    run_command(None, ctx, &words)
}

fn run_command(u: Option<&mut User>, ctx: &mut ServiceContext, words: &[String]) -> Result<()> {
    let (cmd, params) = get(commands(), words)
        .map_err(|e| anyhow!("{e} (type \"help\" for a list of commands)"))?;
    if cmd.admin && !ctx.admin {
        bail!("you must be an admin to use this command");
    }
    if !cmd.global && u.is_none() {
        bail!("this command must be run as a user (try running with user run)");
    }

    let Some(handle) = cmd.handle else {
        if !cmd.children.is_empty() {
            let prefix = &words[..words.len() - params.len()];
            let mut l = Vec::new();
            append_help(&cmd.children, prefix, ctx.admin, u.is_none(), &mut l);
            ctx.print(format!("available commands: {}", l.join(", ")));
            return Ok(());
        }
        logf!(
            ctx.srv.logger,
            "command without handler and subcommands invoked: {}",
            words[0]
        );
        bail!("command {:?} not found", words[0]);
    };

    match handle(u, ctx, params) {
        Err(e) if e.downcast_ref::<FlagError>() == Some(&FlagError::Help) => {
            let name = words[..words.len() - params.len()].join(" ");
            bail!("unsupported flag (type \"help {name}\" for a help message)");
        }
        res => res,
    }
}

fn handle_help(u: Option<&mut User>, ctx: &mut ServiceContext, params: &[String]) -> Result<()> {
    let global = u.is_none();
    if !params.is_empty() {
        let (cmd, rest) = get(commands(), params)?;
        let words = &params[..params.len() - rest.len()];
        if !cmd.children.is_empty() {
            let mut l = Vec::new();
            append_help(&cmd.children, words, ctx.admin, global, &mut l);
            ctx.print(format!("available commands: {}", l.join(", ")));
        } else {
            let mut text = words.join(" ");
            if !cmd.usage.is_empty() {
                text = format!("{text} {}", cmd.usage);
            }
            text = format!("{text}: {}", cmd.desc);
            ctx.print(text);
        }
    } else {
        let mut l = Vec::new();
        append_help(commands(), &[], ctx.admin, global, &mut l);
        ctx.print(format!("available commands: {}", l.join(", ")));
    }
    Ok(())
}

fn parse_flags(fs: &FlagSet, params: &[String]) -> Result<Parsed> {
    Ok(fs.parse(params)?)
}

fn no_args(p: &Parsed) -> Result<()> {
    if let Some(a) = p.args.first() {
        bail!("unexpected argument: {a}");
    }
    Ok(())
}

fn pop_arg(params: &[String]) -> (String, &[String]) {
    match params.first() {
        Some(p) if !p.starts_with('-') => (p.clone(), &params[1..]),
        _ => (String::new(), params),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup() {
        let w = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let words = w("net crea -addr x");
        let (c, rest) = get(commands(), &words).unwrap();
        assert_eq!(c.name, "create");
        assert_eq!(rest, &w("-addr x")[..]);
        let (c, _) = get(commands(), &w("n st")).unwrap();
        assert_eq!(c.name, "status");
        assert!(
            get(commands(), &w("s"))
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert!(get(commands(), &w("nope")).is_err());
        let words = w("channel");
        let (c, rest) = get(commands(), &words).unwrap();
        assert_eq!(c.name, "channel");
        assert!(rest.is_empty());
    }

    #[test]
    fn help_listing() {
        let mut l = Vec::new();
        append_help(commands(), &[], false, false, &mut l);
        assert!(l.contains(&"network create".to_owned()));
        assert!(!l.iter().any(|s| s.starts_with("server")));
        assert!(!l.contains(&"user create".to_owned()));
        let mut l = Vec::new();
        append_help(commands(), &[], true, true, &mut l);
        assert!(l.contains(&"user create".to_owned()));
        assert!(l.contains(&"server status".to_owned()));
        assert!(!l.contains(&"network create".to_owned()));
    }

    #[test]
    fn durations() {
        use std::time::Duration;
        assert_eq!(parse_go_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_go_duration("300s"), Some(Duration::from_secs(300)));
        assert_eq!(parse_go_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_go_duration("2.5h"), Some(Duration::from_secs(9000)));
        assert_eq!(parse_go_duration("-1s"), None);
        assert_eq!(parse_go_duration("10"), None);
        assert_eq!(parse_go_duration("x"), None);
        assert_eq!(
            redact_command("PRIVMSG NickServ :IDENTIFY hunter2"),
            "PRIVMSG NickServ (parameters hidden)"
        );
        assert_eq!(redact_command("OPER x"), "OPER (parameters hidden)");
        assert_eq!(redact_command("AWAY"), "AWAY");
        assert_eq!(parse_go_duration("9999999999999999h"), None);
        assert_eq!(parse_go_duration(&format!("{}s", "9".repeat(400))), None);
        assert!(parse_go_duration("2562047h").is_some());

        assert_eq!(format_go_duration(Duration::ZERO), "0s");
        assert_eq!(format_go_duration(Duration::from_secs(300)), "5m0s");
        assert_eq!(format_go_duration(Duration::from_secs(81000)), "22h30m0s");
        assert_eq!(format_go_duration(Duration::from_millis(1500)), "1.5s");
        for s in ["0s", "5m0s", "22h30m0s", "1h0m1.25s"] {
            assert_eq!(format_go_duration(parse_go_duration(s).unwrap()), s);
        }
    }
}
