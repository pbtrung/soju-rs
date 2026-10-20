//! BouncerServ, the IRC service used to manage the bouncer (port of
//! upstream's service.go).

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use tokio::sync::mpsc;

use crate::database as db;
use crate::downstream::{DownstreamConn, ILLEGAL_CHAN_CHARS};
use crate::flags::{FlagError, FlagSet, Parsed};
use crate::irc::Message;
use crate::ircutil::split_words;
use crate::server::{Server, block_on};
use crate::user::{Event, User, UserRunMsg};
use crate::{logf, msg};

/// Maximum number of RSA key bits used when generating a new private key.
const MAX_RSA_BITS: i64 = 8192;

const DEVICE_CERTIFICATE_DISPLAY_BYTES: usize = 6;

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
}

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
        }
    }

    pub fn for_user(srv: Arc<Server>, admin: bool) -> ServiceContext {
        ServiceContext {
            srv,
            nick: String::new(),
            network_id: None,
            peer_cert: None,
            remote_ip: None,
            admin,
            deadline: None,
            prints: Vec::new(),
        }
    }

    pub fn print(&mut self, s: impl Into<String>) {
        self.prints.push(s.into());
    }

    pub fn take_prints(&mut self) -> Vec<String> {
        std::mem::take(&mut self.prints)
    }

    /// Blocks on a future, e.g. a reply from another user's event loop.
    /// Returns None if the deadline is reached first: two users waiting on
    /// each other would otherwise deadlock.
    fn wait<F: std::future::Future>(&self, fut: F) -> Option<F::Output> {
        match self.deadline {
            Some(d) => block_on(tokio::time::timeout_at(d, fut)).ok(),
            None => Some(block_on(fut)),
        }
    }
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
                        "-addr <addr> [-name name] [-username username] [-pass pass] [-realname realname] [-certfp fingerprint] [-nick nick] [-auto-away auto-away] [-enabled enabled] [-ignore-limit ignore-limit] [-connect-command command]...",
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
                        "[name] [-addr addr] [-name name] [-username username] [-pass pass] [-realname realname] [-certfp fingerprint] [-nick nick] [-auto-away auto-away] [-enabled enabled] [-ignore-limit ignore-limit] [-connect-command command]...",
                        "update a network",
                        handle_network_update,
                    ),
                    Command::leaf("delete", "[name]", "delete a network", handle_network_delete),
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
                        "-username <username> -password <password> [-disable-password] [-admin true|false] [-nick <nick>] [-realname <realname>] [-enabled true|false]  [-max-networks <max-networks>]",
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
            .admin(),
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

/// Returns the network named by the first argument, or the current one.
fn get_network_from_arg<'a>(
    u: &User,
    ctx: &ServiceContext,
    params: &'a [String],
) -> Result<(usize, &'a [String])> {
    let (name, rest) = pop_arg(params);
    if name.is_empty() {
        let Some(ni) = ctx.network_id.and_then(|id| u.net_index(id)) else {
            bail!("no network selected, a name argument is required");
        };
        return Ok((ni, rest));
    }
    let Some(ni) = u.get_network(&name) else {
        bail!("unknown network {name:?}");
    };
    Ok((ni, rest))
}

fn get_network_from_flag(u: &User, ctx: &ServiceContext, name: &str) -> Result<usize> {
    if name.is_empty() {
        return ctx
            .network_id
            .and_then(|id| u.net_index(id))
            .ok_or_else(|| anyhow!("no network selected, -network is required"));
    }
    u.get_network(name)
        .ok_or_else(|| anyhow!("unknown network {name:?}"))
}

fn network_flag_set() -> FlagSet {
    FlagSet::new()
        .string("addr")
        .string("name")
        .string("nick")
        .string("username")
        .string("pass")
        .string("realname")
        .string("certfp")
        .bool_value("auto-away")
        .bool_value("enabled")
        .bool("ignore-limit")
        .string("connect-command")
}

fn update_network_from_flags(p: &Parsed, network: &mut db::Network) -> Result<()> {
    if let Some(addr) = p.get("addr") {
        if let Some((scheme, _)) = addr.split_once("://")
            && !matches!(scheme, "ircs" | "irc+insecure" | "irc+unix")
        {
            bail!("unknown scheme {scheme:?} (supported schemes: ircs, irc+insecure, irc+unix)");
        }
        network.addr = addr.to_owned();
    }
    if let Some(name) = p.get("name") {
        if name == "*" {
            bail!("the network name {name:?} is reserved");
        }
        network.name = name.to_owned();
    }
    if let Some(v) = p.get("nick") {
        network.nick = v.to_owned();
    }
    if let Some(v) = p.get("username") {
        network.username = v.to_owned();
    }
    if let Some(v) = p.get("pass") {
        network.pass = v.to_owned();
    }
    if let Some(v) = p.get("realname") {
        network.realname = v.to_owned();
    }
    if let Some(v) = p.get("certfp") {
        let certfp = v.replace(':', "").to_lowercase();
        if hex::decode(&certfp).is_err() {
            bail!("the certificate fingerprint must be hex-encoded");
        }
        network.certfp = match certfp.len() {
            0 => String::new(),
            64 => format!("sha-256:{certfp}"),
            128 => format!("sha-512:{certfp}"),
            _ => bail!("the certificate fingerprint must be a SHA256 or SHA512 hash"),
        };
    }
    if let Some(v) = p.get_bool("auto-away") {
        network.auto_away = v;
    }
    if let Some(v) = p.get_bool("enabled") {
        network.enabled = v;
    }
    if let Some(cmds) = p.get_all("connect-command") {
        if cmds.len() == 1 && cmds[0].is_empty() {
            network.connect_commands.clear();
        } else {
            if cmds.len() > 20 {
                bail!("too many -connect-command flags supplied");
            }
            for command in cmds {
                if let Err(e) = Message::parse(command) {
                    bail!(
                        "flag -connect-command must be a valid raw irc command string: {command:?}: {e}"
                    );
                }
            }
            network.connect_commands = cmds.to_vec();
        }
    }
    Ok(())
}

fn handle_network_create(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&network_flag_set(), params)?;
    no_args(&p)?;
    let Some(addr) = p.get("addr") else {
        bail!("flag -addr is required");
    };
    let ignore_limit = p.get_bool("ignore-limit").unwrap_or(false);
    if ignore_limit && !ctx.admin {
        bail!("you must be an admin to use the flag -ignore-limit");
    }
    let mut record = db::Network::new(addr);
    update_network_from_flags(&p, &mut record)?;
    let ni = u
        .create_network(record, !ignore_limit)
        .map_err(|e| anyhow!("could not create network: {e}"))?;
    ctx.print(format!("created network {:?}", u.networks[ni].get_name()));
    Ok(())
}

fn handle_network_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if !params.is_empty() {
        bail!("expected no argument");
    }
    let mut n = 0;
    for net in &u.networks {
        let mut statuses = Vec::new();
        let mut details = String::new();
        if let Some(uc) = net.upstream() {
            if !ctx.nick.is_empty() && ctx.nick != uc.nick {
                statuses.push(format!("connected as {}", uc.nick));
            } else {
                statuses.push("connected".to_owned());
            }
            details = format!("{} channels", uc.channels.len());
        } else if !net.record.enabled {
            statuses.push("disabled".to_owned());
        } else {
            statuses.push("disconnected".to_owned());
            if let Some(e) = &net.last_error {
                details = e.clone();
            }
        }
        if Some(net.id()) == ctx.network_id {
            statuses.push("current".to_owned());
        }
        let mut name = net.get_name().to_owned();
        if name != net.record.addr {
            name = format!("{name} ({})", net.record.addr);
        }
        let mut s = format!("{name} [{}]", statuses.join(", "));
        if !details.is_empty() {
            s = format!("{s}: {details}");
        }
        ctx.print(s);
        n += 1;
    }
    if n == 0 {
        ctx.print(r#"No network configured, add one with "network create"."#);
    }
    Ok(())
}

fn handle_network_info(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let (ni, rest) = get_network_from_arg(u, ctx, params)?;
    if let Some(a) = rest.first() {
        bail!("unexpected argument: {a}");
    }
    let net = &u.networks[ni].record;
    ctx.print(format!("name: {}", net.get_name()));
    ctx.print(format!("addr: {}", net.addr));
    ctx.print(format!("nick: {}", db::get_nick(&u.record, Some(net))));
    ctx.print(format!(
        "username: {}",
        db::get_username(&u.record, Some(net))
    ));
    ctx.print(format!(
        "realname: {}",
        db::get_realname(&u.record, Some(net))
    ));
    // Secrets are never printed
    if !net.pass.is_empty() {
        ctx.print("pass: (set)");
    }
    if !net.certfp.is_empty() {
        ctx.print(format!("certfp: {}", net.certfp));
    }
    match net.sasl.mechanism.as_str() {
        "" => ctx.print("sasl: none"),
        "PLAIN" => ctx.print(format!(
            "sasl: PLAIN (username: {})",
            net.sasl.plain_username
        )),
        mech => ctx.print(format!("sasl: {mech}")),
    }
    ctx.print(format!("auto-away: {}", net.auto_away));
    ctx.print(format!("enabled: {}", net.enabled));
    for cmd in &net.connect_commands {
        ctx.print(format!("connect-command: {}", redact_command(cmd)));
    }
    Ok(())
}

/// Shows a raw IRC command without its last parameters, which may contain
/// secrets, e.g. "PRIVMSG NickServ :IDENTIFY <password>".
fn redact_command(raw: &str) -> String {
    let Ok(m) = Message::parse(raw) else {
        return "(invalid)".to_owned();
    };
    match m.params.first() {
        Some(target) if m.params.len() > 1 => format!("{} {target} (parameters hidden)", m.command),
        Some(_) => format!("{} (parameters hidden)", m.command),
        None => m.command,
    }
}

fn handle_network_update(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let (ni, params) = get_network_from_arg(u, ctx, params)?;
    let p = parse_flags(&network_flag_set(), params)?;
    no_args(&p)?;
    let ignore_limit = p.get_bool("ignore-limit").unwrap_or(false);
    if ignore_limit && !ctx.admin {
        bail!("you must be an admin to use the flag -ignore-limit");
    }
    let mut record = u.networks[ni].record.clone();
    let was_enabled = record.enabled;
    update_network_from_flags(&p, &mut record)?;
    let ni = u
        .update_network(record, !ignore_limit && !was_enabled)
        .map_err(|e| anyhow!("could not update network: {e}"))?;
    ctx.print(format!("updated network {:?}", u.networks[ni].get_name()));
    Ok(())
}

fn handle_network_delete(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let (ni, _) = get_network_from_arg(u, ctx, params)?;
    let id = u.networks[ni].id();
    let name = u.networks[ni].get_name().to_owned();
    u.delete_network(id)?;
    ctx.print(format!("deleted network {name:?}"));
    Ok(())
}

fn handle_network_quote(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 && params.len() != 2 {
        bail!("expected one or two arguments");
    }
    let raw = params.last().unwrap().clone();
    let (ni, _) = get_network_from_arg(u, ctx, &params[..params.len() - 1])?;
    let name = u.networks[ni].get_name().to_owned();
    let Some(uc) = u.networks[ni].upstream() else {
        bail!("network {name:?} is not currently connected");
    };
    let m = Message::parse(&raw).map_err(|e| anyhow!("failed to parse command {raw:?}: {e}"))?;
    uc.send_message(m);
    ctx.print(format!("sent command to {name:?}"));
    Ok(())
}

fn send_certfp_fingerprints(ctx: &mut ServiceContext, cert: &[u8]) {
    ctx.print(format!(
        "SHA-1 fingerprint: {}",
        hex::encode(Sha1::digest(cert))
    ));
    ctx.print(format!(
        "SHA-256 fingerprint: {}",
        hex::encode(Sha256::digest(cert))
    ));
    ctx.print(format!(
        "SHA-512 fingerprint: {}",
        hex::encode(Sha512::digest(cert))
    ));
}

fn handle_certfp_generate(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(
        &FlagSet::new()
            .string("network")
            .string("key-type")
            .int("bits"),
        params,
    )?;
    no_args(&p)?;
    let key_type = p.string_or("key-type", "rsa");
    let bits = p.get_int("bits").unwrap_or(3072);
    if bits <= 0 || bits > MAX_RSA_BITS {
        bail!("invalid value for -bits");
    }
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let (key, cert) = crate::certfp::generate_certfp(&key_type, bits as usize)?;
    let net = &mut u.networks[ni];
    net.record.sasl.external_cert = Some(cert.clone());
    net.record.sasl.external_key = Some(key);
    net.record.sasl.mechanism = "EXTERNAL".to_owned();
    net.sync_snapshot();
    ctx.srv.db.store_network(u.record.id, &mut net.record)?;
    ctx.print("certificate generated");
    send_certfp_fingerprints(ctx, &cert);
    Ok(())
}

fn handle_certfp_fingerprints(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let sasl = &u.networks[ni].record.sasl;
    if sasl.mechanism != "EXTERNAL" {
        bail!("CertFP not set up");
    }
    let cert = sasl.external_cert.clone().unwrap_or_default();
    send_certfp_fingerprints(ctx, &cert);
    Ok(())
}

fn handle_sasl_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let net = &u.networks[ni];
    match net.record.sasl.mechanism.as_str() {
        "PLAIN" => ctx.print(format!(
            "SASL PLAIN enabled with username {:?}",
            net.record.sasl.plain_username
        )),
        "EXTERNAL" => ctx.print("SASL EXTERNAL (CertFP) enabled"),
        "" => ctx.print("SASL is disabled"),
        _ => {}
    }
    match net.upstream() {
        Some(uc) if !uc.account.is_empty() => ctx.print(format!(
            "Authenticated on upstream network with account {:?}",
            uc.account
        )),
        Some(_) => ctx.print("Unauthenticated on upstream network"),
        None => ctx.print("Disconnected from upstream network"),
    }
    Ok(())
}

fn handle_sasl_set_plain(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    if p.args.len() != 2 {
        bail!("expected exactly 2 arguments");
    }
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let net = &mut u.networks[ni];
    net.record.sasl.plain_username = p.args[0].clone();
    net.record.sasl.plain_password = p.args[1].clone();
    net.record.sasl.mechanism = "PLAIN".to_owned();
    ctx.srv.db.store_network(u.record.id, &mut net.record)?;
    net.sync_snapshot();
    ctx.print("credentials saved");
    Ok(())
}

fn handle_sasl_reset(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let net = &mut u.networks[ni];
    net.record.sasl = db::Sasl::default();
    net.sync_snapshot();
    ctx.srv.db.store_network(u.record.id, &mut net.record)?;
    ctx.print("credentials reset");
    Ok(())
}

fn handle_user_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if params.len() > 1 {
        bail!("expected 0 or 1 argument");
    }
    // Limit to a small amount of users to avoid sending thousands of
    // messages on large instances.
    let mut users: Vec<db::User> = Vec::new();
    let n;
    let own = u.map(|u| u.record.clone());
    if params.is_empty() {
        let all = ctx.srv.user_records();
        n = all.len();
        users.extend(all.into_iter().take(50));
    } else {
        let username = &params[0];
        let record = match &own {
            Some(r) if r.username == *username => Some(r.clone()),
            _ => ctx.srv.get_user(username).map(|h| h.record()),
        };
        let Some(r) = record else {
            bail!("unknown username {username:?}");
        };
        users.push(r);
        n = 1;
    }
    for mut user in users {
        if let Some(own) = &own
            && own.username == user.username
        {
            user = own.clone();
        }
        let mut attrs = Vec::new();
        if user.admin {
            attrs.push("admin");
        }
        if !user.enabled {
            attrs.push("disabled");
        }
        let mut line = user.username.clone();
        if !attrs.is_empty() {
            line = format!("{line} ({})", attrs.join(", "));
        }
        let networks = ctx
            .srv
            .db
            .list_networks(user.id)
            .map_err(|e| anyhow!("could not get networks of user {:?}: {e}", user.username))?;
        line = format!("{line}: {} networks", networks.len());
        if user.max_networks >= 0 {
            line = format!("{line} ({} max)", user.max_networks);
        }
        ctx.print(line);
    }
    if n > 50 {
        ctx.print(format!("({} more users omitted)", n - 50));
    }
    Ok(())
}

fn handle_user_create(
    _u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let fs = FlagSet::new()
        .string("username")
        .string("password")
        .bool("disable-password")
        .string("nick")
        .string("realname")
        .bool("admin")
        .bool("enabled")
        .int("max-networks");
    let p = parse_flags(&fs, params)?;
    no_args(&p)?;
    let username = p.string_or("username", "");
    let password = p.string_or("password", "");
    let disable_password = p.get_bool("disable-password").unwrap_or(false);
    if username.is_empty() {
        bail!("flag -username is required");
    }
    if !password.is_empty() && disable_password {
        bail!("flags -password and -disable-password are mutually exclusive");
    }
    if password.is_empty() && !disable_password {
        bail!("flag -password is required");
    }
    let mut user = db::User::new(&username);
    user.nick = p.string_or("nick", "");
    user.realname = p.string_or("realname", "");
    user.admin = p.get_bool("admin").unwrap_or(false);
    user.enabled = p.get_bool("enabled").unwrap_or(true);
    user.max_networks = p.get_int("max-networks").unwrap_or(-1);
    if !disable_password {
        user.set_password(&password)?;
    }
    ctx.srv
        .create_user(user)
        .map_err(|e| anyhow!("could not create user: {e}"))?;
    ctx.print(format!("created user {username:?}"));
    Ok(())
}

fn handle_user_update(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let fs = FlagSet::new()
        .string("password")
        .bool("disable-password")
        .string("nick")
        .string("realname")
        .bool_value("admin")
        .bool_value("enabled")
        .atoi("max-networks");
    let (username, params) = pop_arg(params);
    let p = parse_flags(&fs, params)?;
    no_args(&p)?;
    if username.is_empty() && u.is_none() {
        bail!("cannot determine the user to update");
    }
    let password = p.get("password").map(str::to_owned);
    let disable_password = p.get_bool("disable-password").unwrap_or(false);
    let nick = p.get("nick").map(str::to_owned);
    let realname = p.get("realname").map(str::to_owned);
    let admin = p.get_bool("admin");
    let enabled = p.get_bool("enabled");
    let max_networks = p.get_int("max-networks");
    if password.is_some() && disable_password {
        bail!("flags -password and -disable-password are mutually exclusive");
    }

    let is_self = u.as_ref().is_some_and(|u| u.record.username == username);
    if !username.is_empty() && !is_self {
        if !ctx.admin {
            bail!("you must be an admin to update other users");
        }
        if nick.is_some() {
            bail!("cannot update -nick of other user");
        }
        if realname.is_some() {
            bail!("cannot update -realname of other user");
        }
        let mut hashed = None;
        if let Some(pw) = &password {
            let mut record = db::User::new("");
            record.set_password(pw)?;
            hashed = Some(record.password);
        }
        if disable_password {
            hashed = Some(String::new());
        }
        let Some(handle) = ctx.srv.get_user(&username) else {
            bail!("unknown username {username:?}");
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if !handle.send(Event::UserUpdate {
            password: hashed,
            admin,
            enabled,
            max_networks,
            done: tx,
        }) {
            bail!("unknown username {username:?}");
        }
        // TODO: send context to the other side
        ctx.wait(rx)
            .ok_or_else(|| anyhow!("context deadline exceeded"))?
            .map_err(|_| anyhow!("timeout executing command"))?
            .map_err(|e| anyhow!(e))?;
        ctx.print(format!("updated user {username:?}"));
    } else {
        let u = u.unwrap();
        if admin.is_some() {
            bail!("cannot update -admin of own user");
        }
        if enabled.is_some() {
            bail!("cannot update -enabled of own user");
        }
        if max_networks.is_some() && !ctx.admin {
            bail!("cannot update -max-networks of own user");
        }
        u.update_user(|record| {
            if let Some(pw) = &password {
                record.set_password(pw)?;
            }
            if disable_password {
                record.password.clear();
            }
            if let Some(n) = &nick {
                record.nick = n.clone();
            }
            if let Some(r) = &realname {
                record.realname = r.clone();
            }
            if let Some(m) = max_networks {
                record.max_networks = m;
            }
            Ok(())
        })?;
        ctx.print(format!("updated user {:?}", u.record.username));
    }
    Ok(())
}

fn handle_user_delete(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if params.len() != 1 && params.len() != 2 {
        bail!("expected one or two arguments");
    }
    let username = params[0].clone();
    let hash = hex::encode(&Sha1::digest(username.as_bytes())[..3]);
    let is_self = u.as_ref().is_some_and(|u| u.record.username == username);
    if !ctx.admin && !is_self {
        bail!("only admins may delete other users");
    }
    let Some(handle) = ctx.srv.get_user(&username) else {
        bail!("unknown username {username:?}");
    };
    if params.len() < 2 {
        ctx.print(format!(
            r#"To confirm user deletion, send "user delete {username} {hash}""#
        ));
        return Ok(());
    }
    if params[1] != hash {
        bail!("provided confirmation token doesn't match user");
    }

    let user_id = handle.record().id;
    if is_self {
        ctx.print(format!(
            "Goodbye {username}, deleting your account. There will be no further confirmation."
        ));
        // We can't wait for our own event loop to stop from within it: stop
        // asynchronously, then delete the user
        let srv = ctx.srv.clone();
        tokio::spawn(async move {
            handle.stop().await;
            if let Err(e) = db::run(&srv.db, move |db| db.delete_user(user_id)).await {
                logf!(srv.logger, "failed to delete user {username:?}: {e}");
            }
        });
        return Ok(());
    }

    if ctx.wait(handle.stop()).is_none() {
        bail!("failed to stop user: context deadline exceeded");
    }
    ctx.srv
        .db
        .delete_user(user_id)
        .map_err(|e| anyhow!("failed to delete user: {e}"))?;
    ctx.print(format!("deleted user {username:?}"));
    Ok(())
}

fn handle_user_run(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if params.len() < 2 {
        bail!("expected at least two arguments");
    }
    let username = params[0].clone();
    let params = &params[1..];
    if let Some(u) = u
        && u.record.username == username
    {
        return handle_service_command(u, ctx, params);
    }
    let Some(handle) = ctx.srv.get_user(&username) else {
        bail!("unknown username {username:?}");
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    if !handle.send(Event::UserRun {
        params: params.to_vec(),
        tx,
    }) {
        bail!("unknown username {username:?}");
    }
    loop {
        match ctx.wait(rx.recv()).flatten() {
            Some(UserRunMsg::Message(m)) => ctx.print(m),
            Some(UserRunMsg::Done(res)) => return res.map_err(|e| anyhow!(e)),
            None => bail!("timeout executing command"),
        }
    }
}

fn handle_channel_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let default_name = ctx
        .network_id
        .and_then(|id| u.net_index(id))
        .map(|ni| u.networks[ni].get_name().to_owned())
        .unwrap_or_default();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let network_name = p.string_or("network", &default_name);

    let mut n = 0;
    let mut send_network = |ctx: &mut ServiceContext, net: &crate::user::Network| {
        let mut channels: Vec<&db::Channel> = net.channels.values().collect();
        channels.sort_by_key(|c| c.name.replace('#', ""));
        for ch in channels {
            let uc = net.upstream();
            let mut name = ch.name.clone();
            if network_name.is_empty() {
                name = format!("{name}/{}", net.get_name());
            }
            let mut status = match uc {
                Some(uc) if uc.channels.has(&ch.name) => "joined",
                Some(_) => "parted",
                None => "disconnected",
            }
            .to_owned();
            if ch.detached {
                status.push_str(", detached");
            }
            ctx.print(format!(
                "{name} [{status}]: relay-detached: {}, reattach-on: {}, detach-after: {}, detach-on: {}",
                filter_name(ch.relay_detached),
                filter_name(ch.reattach_on),
                format_go_duration(ch.detach_after),
                filter_name(ch.detach_on),
            ));
            n += 1;
        }
    };

    if network_name.is_empty() {
        for net in &u.networks {
            send_network(ctx, net);
        }
    } else {
        let Some(ni) = u.get_network(&network_name) else {
            bail!("unknown network {network_name:?}");
        };
        send_network(ctx, &u.networks[ni]);
    }
    if n == 0 {
        ctx.print("No channel configured.");
    }
    Ok(())
}

fn parse_filter(filter: &str) -> Result<db::MessageFilter> {
    Ok(match filter {
        "default" => db::MessageFilter::Default,
        "none" => db::MessageFilter::None,
        "highlight" => db::MessageFilter::Highlight,
        "message" => db::MessageFilter::Message,
        _ => bail!("unknown filter: {filter:?}"),
    })
}

fn filter_name(filter: db::MessageFilter) -> &'static str {
    match filter {
        db::MessageFilter::Default => "default",
        db::MessageFilter::None => "none",
        db::MessageFilter::Highlight => "highlight",
        db::MessageFilter::Message => "message",
    }
}

fn channel_flag_set() -> FlagSet {
    FlagSet::new()
        .bool_value("detached")
        .string("relay-detached")
        .string("reattach-on")
        .string("detach-after")
        .string("detach-on")
}

fn update_channel_from_flags(p: &Parsed, ch: &mut db::Channel) -> Result<()> {
    if let Some(v) = p.get("relay-detached") {
        ch.relay_detached = parse_filter(v)?;
    }
    if let Some(v) = p.get("reattach-on") {
        ch.reattach_on = parse_filter(v)?;
    }
    if let Some(v) = p.get("detach-after") {
        ch.detach_after = parse_go_duration(v).ok_or_else(|| {
            anyhow!(
                "unknown duration for -detach-after {v:?} (duration format: 0, 300s, 22h30m, ...)"
            )
        })?;
    }
    if let Some(v) = p.get("detach-on") {
        ch.detach_on = parse_filter(v)?;
    }
    Ok(())
}

/// Parses a duration like Go's time.ParseDuration. Negative durations are
/// rejected.
pub fn parse_go_duration(s: &str) -> Option<std::time::Duration> {
    let mut s = s;
    if s == "0" {
        return Some(std::time::Duration::ZERO);
    }
    if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    } else if s.starts_with('-') {
        return None;
    }
    if s.is_empty() {
        return None;
    }
    let mut total = 0f64;
    while !s.is_empty() {
        let num_len = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len());
        if num_len == 0 {
            return None;
        }
        let v: f64 = s[..num_len].parse().ok()?;
        s = &s[num_len..];
        let unit_len = s
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(s.len());
        let unit = &s[..unit_len];
        s = &s[unit_len..];
        let mult = match unit {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += v * mult;
    }
    // Like Go's time.Duration, an int64 of nanoseconds
    if total > MAX_GO_DURATION_SECS {
        return None;
    }
    std::time::Duration::try_from_secs_f64(total).ok()
}

/// The largest Go time.Duration, in seconds (about 292 years).
pub const MAX_GO_DURATION_SECS: f64 = i64::MAX as f64 / 1e9;

/// Formats a duration like Go's time.Duration.String, e.g. "1h30m0s", so that
/// it can be passed back to parse_go_duration.
pub fn format_go_duration(d: std::time::Duration) -> String {
    if d.is_zero() {
        return "0s".to_owned();
    }
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&s.to_string());
    let nanos = d.subsec_nanos();
    if nanos > 0 {
        let frac = format!("{nanos:09}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out.push('s');
    out
}

/// Splits "name/network" when the caller isn't bound to a network.
fn strip_network_suffix(u: &User, ctx: &ServiceContext, name: &str) -> Result<(String, usize)> {
    if let Some(ni) = ctx.network_id.and_then(|id| u.net_index(id)) {
        return Ok((name.to_owned(), ni));
    }
    let Some((name, net_name)) = name.split_once('/') else {
        bail!("missing network name");
    };
    match u.networks.iter().position(|n| n.get_name() == net_name) {
        Some(ni) => Ok((name.to_owned(), ni)),
        None => bail!("unknown network {net_name:?}"),
    }
}

fn handle_channel_create(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.is_empty() {
        bail!("expected at least one argument");
    }
    let p = parse_flags(&channel_flag_set(), &params[1..])?;
    no_args(&p)?;
    let (name, ni) = strip_network_suffix(u, ctx, &params[0])?;
    if name.is_empty() || name.contains(ILLEGAL_CHAN_CHARS) {
        bail!("invalid channel name: {name}");
    }
    let net = &mut u.networks[ni];
    if let Some(uc) = net.upstream()
        && !uc.is_channel(&name)
    {
        bail!("not a channel name: {name}");
    }
    if net.channels.has(&name) {
        bail!("channel {name:?} already exists");
    }
    let mut ch = db::Channel {
        name: name.clone(),
        ..Default::default()
    };
    update_channel_from_flags(&p, &mut ch)?;
    let net_id = net.id();
    net.channels.set(&name, ch);
    let ch = net.channels.get_mut(&name).unwrap();
    ctx.srv
        .db
        .store_channel(net_id, ch)
        .map_err(|e| anyhow!("failed to create channel: {e}"))?;
    if let Some(uc) = net.upstream() {
        uc.send_message(msg!("JOIN", name.clone()));
    }
    ctx.print(format!("created channel {name:?}"));
    Ok(())
}

fn handle_channel_update(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.is_empty() {
        bail!("expected at least one argument");
    }
    let p = parse_flags(&channel_flag_set(), &params[1..])?;
    no_args(&p)?;
    let (name, ni) = strip_network_suffix(u, ctx, &params[0])?;
    let Some(ch) = u.networks[ni].channels.get_mut(&name) else {
        bail!("unknown channel {name:?}");
    };
    update_channel_from_flags(&p, ch)?;
    let detached = ch.detached;
    if let Some(d) = p.get_bool("detached")
        && d != detached
    {
        if d {
            u.detach_channel(ni, &name);
        } else {
            u.attach_channel(ni, &name);
        }
    }
    crate::upstream::update_channel_auto_detach(u, ni, &name);
    let net_id = u.networks[ni].id();
    let ch = u.networks[ni].channels.get_mut(&name).unwrap();
    ctx.srv
        .db
        .store_channel(net_id, ch)
        .map_err(|e| anyhow!("failed to update channel: {e}"))?;
    ctx.print(format!("updated channel {name:?}"));
    Ok(())
}

fn handle_channel_delete(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let (name, ni) = strip_network_suffix(u, ctx, &params[0])?;
    u.delete_channel(ni, &name)
        .map_err(|e| anyhow!("failed to delete channel: {e}"))?;
    if let Some(uc) = u.networks[ni].upstream()
        && uc.channels.has(&name)
    {
        uc.send_message(msg!("PART", name.clone()));
    }
    ctx.print(format!("deleted channel {name:?}"));
    Ok(())
}

fn colon_hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn handle_device_cert_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if !params.is_empty() {
        bail!("expected no argument");
    }
    let certs = ctx
        .srv
        .db
        .list_device_certificates(u.record.id)
        .map_err(|e| anyhow!("could not get device certificates: {e}"))?;
    if certs.is_empty() {
        ctx.print("No device certificate configured.");
        return Ok(());
    }
    for cert in certs {
        let n = DEVICE_CERTIFICATE_DISPLAY_BYTES.min(cert.fingerprint.len());
        let last_used = cert
            .last_used
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "0001-01-01 00:00:00".to_owned());
        ctx.print(format!(
            "{} [{}] [{}] [{}]",
            colon_hex(&cert.fingerprint[..n]),
            cert.last_ip,
            cert.label,
            last_used
        ));
    }
    Ok(())
}

fn handle_device_cert_create(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if !ctx.srv.config().basic.client_cert_auth {
        bail!("client certification authentication is disabled");
    }
    let p = parse_flags(
        &FlagSet::new().string("fingerprint").string("label"),
        params,
    )?;
    no_args(&p)?;
    let mut fingerprint = p.string_or("fingerprint", "");
    let label = p.string_or("label", "");
    if fingerprint.is_empty()
        && let Some(cert) = &ctx.peer_cert
    {
        fingerprint = colon_hex(&Sha512::digest(cert));
    }
    if fingerprint.is_empty() {
        bail!("flag -fingerprint is required when no TLS client certificate is used");
    }
    if label.is_empty() {
        bail!("flag -label is required");
    }
    let h = match hex::decode(fingerprint.replace(':', "")) {
        Ok(h) if h.len() == 64 => h,
        _ => bail!("the certificate fingerprint must be hex-encoded SHA-512"),
    };
    let mut cert = db::DeviceCertificate {
        label,
        fingerprint: h,
        ..Default::default()
    };
    cert.mark_used(ctx.remote_ip.map(|ip| std::net::SocketAddr::new(ip, 0)));
    match ctx.srv.db.store_device_certificate(u.record.id, &mut cert) {
        Ok(()) => {}
        Err(e) if e.is::<db::DuplicateDeviceCertificate>() => {
            bail!("this device certificate fingerprint already exists")
        }
        Err(e) => bail!("could not create device certificate: {e}"),
    }
    ctx.print(format!("created device certificate {fingerprint:?}"));
    Ok(())
}

fn handle_device_cert_delete(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let fingerprint = &params[0];
    let h = match hex::decode(fingerprint.replace(':', "")) {
        Ok(h) if h.len() <= 64 && h.len() >= DEVICE_CERTIFICATE_DISPLAY_BYTES => h,
        _ => bail!("the certificate fingerprint must be hex-encoded SHA-256"),
    };
    let certs = ctx
        .srv
        .db
        .list_device_certificates(u.record.id)
        .map_err(|e| anyhow!("could not get device certificate: {e}"))?;
    let mut found: Option<db::DeviceCertificate> = None;
    for cert in certs {
        if cert.fingerprint.starts_with(&h) {
            if found.is_some() {
                bail!(
                    "multiple device certificates match this fingerprint prefix; pass the full fingerprint"
                );
            }
            found = Some(cert);
        }
    }
    let Some(c) = found else {
        bail!("no device certificate found for fingerprint {fingerprint:?}");
    };
    ctx.srv
        .db
        .delete_device_certificate(u.record.id, &c.fingerprint)
        .map_err(|e| anyhow!("could not delete device certificate: {e}"))?;
    ctx.print(format!("deleted device certificate {fingerprint:?}"));
    Ok(())
}

fn handle_server_status(
    _u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if !params.is_empty() {
        bail!("expected no argument");
    }
    let db_stats = ctx.srv.db.stats()?;
    let s = ctx.srv.stats();
    ctx.print(format!(
        "{}/{} users, {} downstreams, {} upstreams, {} networks, {} channels",
        s.users, db_stats.users, s.downstreams, s.upstreams, db_stats.networks, db_stats.channels
    ));
    Ok(())
}

fn handle_server_notice(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let text = &params[0];
    let logger = match &u {
        Some(u) => u.logger.clone(),
        None => ctx.srv.logger.clone(),
    };
    logf!(logger, "broadcasting bouncer-wide NOTICE: {text}");

    let hostname = ctx.srv.config().basic.hostname.clone();
    let m = msg!("NOTICE", format!("${hostname}"), text.clone())
        .with_prefix(crate::downstream::service_prefix());
    let mut sent = 0;
    let mut total = 0;
    ctx.srv.for_each_user(|h| {
        total += 1;
        if h.send(Event::Broadcast(m.clone())) {
            sent += 1;
        }
    });
    logf!(
        logger,
        "broadcast bouncer-wide NOTICE to {sent}/{total} downstreams"
    );
    ctx.print(format!("sent to {sent}/{total} downstream connections"));
    Ok(())
}

fn handle_server_debug(
    _u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let enabled = match params[0].as_str() {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => true,
        "0" | "f" | "F" | "FALSE" | "false" | "False" => false,
        v => bail!("strconv.ParseBool: parsing {v:?}: invalid syntax"),
    };
    let previous = ctx.srv.logger.set_debug(enabled);
    if previous != enabled {
        if enabled {
            logf!(ctx.srv.logger, "enabling debug logging");
        } else {
            logf!(ctx.srv.logger, "disabling debug logging");
        }
    }
    Ok(())
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
