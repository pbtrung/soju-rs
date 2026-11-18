//! Declarative service commands, typed dispatch and help/abbreviation lookup.

use super::auth::{
    handle_certfp_fingerprints, handle_certfp_generate, handle_device_cert_create,
    handle_device_cert_delete, handle_device_cert_status, handle_sasl_reset, handle_sasl_set_plain,
    handle_sasl_status,
};
use super::channel::{
    handle_channel_create, handle_channel_delete, handle_channel_status, handle_channel_update,
};
use super::network::{
    handle_network_create, handle_network_delete, handle_network_info, handle_network_quote,
    handle_network_status, handle_network_update,
};
use super::server::{handle_server_debug, handle_server_notice, handle_server_status};
use super::user::{
    handle_user_create, handle_user_delete, handle_user_run, handle_user_status, handle_user_update,
};
use super::{ServiceContext, User, handle_help};
use anyhow::{Result, anyhow, bail};

type UserHandler = fn(&mut User, &mut ServiceContext, &[String]) -> Result<()>;
type GlobalHandler = fn(Option<&mut User>, &mut ServiceContext, &[String]) -> Result<()>;

#[derive(Clone, Copy, Debug)]
pub(super) enum Handler {
    User(UserHandler),
    Global(GlobalHandler),
}

impl Handler {
    pub(super) fn run(
        self,
        user: Option<&mut User>,
        ctx: &mut ServiceContext,
        params: &[String],
    ) -> Result<()> {
        match self {
            Self::User(handle) => handle(
                user.ok_or_else(|| {
                    anyhow!("this command must be run as a user (try running with user run)")
                })?,
                ctx,
                params,
            ),
            Self::Global(handle) => handle(user, ctx, params),
        }
    }
}

#[derive(Debug)]
pub(super) struct Command {
    pub(super) name: &'static str,
    pub(super) usage: &'static str,
    pub(super) desc: &'static str,
    pub(super) handle: Option<Handler>,
    pub(super) children: Vec<Command>,
    pub(super) admin: bool,
    pub(super) global: bool,
}

impl Command {
    const fn leaf(
        name: &'static str,
        usage: &'static str,
        desc: &'static str,
        handle: UserHandler,
    ) -> Command {
        Command {
            name,
            usage,
            desc,
            handle: Some(Handler::User(handle)),
            children: Vec::new(),
            admin: false,
            global: false,
        }
    }

    const fn global_leaf(
        name: &'static str,
        usage: &'static str,
        desc: &'static str,
        handle: GlobalHandler,
    ) -> Command {
        Command {
            name,
            usage,
            desc,
            handle: Some(Handler::Global(handle)),
            children: Vec::new(),
            admin: false,
            global: true,
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

pub(super) fn commands() -> &'static [Command] {
    use std::sync::OnceLock;
    static COMMANDS: OnceLock<Vec<Command>> = OnceLock::new();
    COMMANDS.get_or_init(|| {
        vec![
            Command::global_leaf("help", "[command]", "print help message", handle_help),
            network_commands(),
            certfp_commands(),
            sasl_commands(),
            user_commands(),
            channel_commands(),
            device_certificate_commands(),
            server_commands(),
        ]
    })
}

fn network_commands() -> Command {
    Command::group(
        "network",
        vec![
            Command::leaf(
                "create",
                "-addr <addr> [-name name] [-username username] [-pass pass] [-realname \
                            realname] [-certfp fingerprint] [-nick nick] [-auto-away auto-away] \
                            [-enabled enabled] [-ignore-limit] [-connect-command command]...",
                "add a new network",
                handle_network_create,
            ),
            Command::leaf(
                "status",
                "",
                "show a list of saved networks and their current status",
                handle_network_status,
            ),
            Command::leaf(
                "info",
                "[name]",
                "show the configuration of a network",
                handle_network_info,
            ),
            Command::leaf(
                "update",
                "[name] [-addr addr] [-name name] [-username username] [-pass pass] \
                            [-realname realname] [-certfp fingerprint] [-nick nick] [-auto-away \
                            auto-away] [-enabled enabled] [-ignore-limit] [-connect-command \
                            command]...",
                "update a network",
                handle_network_update,
            ),
            Command::leaf(
                "delete",
                "<name>",
                "delete a network",
                handle_network_delete,
            ),
            Command::leaf(
                "quote",
                "[name] <command>",
                "send a raw line to a network",
                handle_network_quote,
            ),
        ],
    )
}

fn certfp_commands() -> Command {
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
    )
}

fn sasl_commands() -> Command {
    Command::group(
        "sasl",
        vec![
            Command::leaf(
                "status",
                "[-network name]",
                "show SASL status",
                handle_sasl_status,
            ),
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
    )
}

fn user_commands() -> Command {
    Command::group(
        "user",
        vec![
            Command::global_leaf(
                "status",
                "[username]",
                "show a list of users and their current \
                        status",
                handle_user_status,
            )
            .admin()
            .global(),
            Command::global_leaf(
                "create",
                "-username <username> -password <password> [-disable-password] [-admin] \
                            [-nick <nick>] [-realname <realname>] [-enabled=false] [-max-networks \
                            <max-networks>]",
                "create a new soju user",
                handle_user_create,
            )
            .admin()
            .global(),
            Command::global_leaf(
                "update",
                "[username] [-password <password>] [-disable-password] [-admin true|false] \
                            [-nick <nick>] [-realname <realname>] [-enabled true|false] \
                            [-max-networks <max-networks>]",
                "update a user",
                handle_user_update,
            )
            .global(),
            Command::global_leaf(
                "delete",
                "<username> [confirmation token]",
                "delete a user",
                handle_user_delete,
            )
            .global(),
            Command::global_leaf(
                "run",
                "<username> <command>",
                "run a command as another user",
                handle_user_run,
            )
            .admin()
            .global(),
        ],
    )
    .global()
}

fn channel_commands() -> Command {
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
                "<name> [-detached true|false] [-relay-detached \
                            default|none|highlight|message] [-reattach-on \
                            default|none|highlight|message] [-detach-after <duration>] [-detach-on \
                            default|none|highlight|message]",
                "create a channel",
                handle_channel_create,
            ),
            Command::leaf(
                "update",
                "<name> [-detached true|false] [-relay-detached \
                            default|none|highlight|message] [-reattach-on \
                            default|none|highlight|message] [-detach-after <duration>] [-detach-on \
                            default|none|highlight|message]",
                "update a channel",
                handle_channel_update,
            ),
            Command::leaf(
                "delete",
                "<name>",
                "delete a channel",
                handle_channel_delete,
            ),
        ],
    )
}

fn device_certificate_commands() -> Command {
    Command::group(
        "device-certificate",
        vec![
            Command::leaf(
                "status",
                "",
                "show a list of device certificates",
                handle_device_cert_status,
            ),
            Command::leaf(
                "create",
                "-fingerprint <fingerprint> -label <label>",
                "create a new device certificate",
                handle_device_cert_create,
            ),
            Command::leaf(
                "delete",
                "<fingerprint>",
                "delete a device certificate",
                handle_device_cert_delete,
            ),
        ],
    )
}

fn server_commands() -> Command {
    Command::group(
        "server",
        vec![
            Command::global_leaf("status", "", "show server statistics", handle_server_status)
                .admin()
                .global(),
            Command::global_leaf(
                "notice",
                "<notice>",
                "broadcast a notice to all connected bouncer users",
                handle_server_notice,
            )
            .admin()
            .global(),
            Command::global_leaf(
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
    .global()
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
pub(super) fn get<'a, 'b>(
    cmds: &'a [Command],
    params: &'b [String],
) -> Result<(&'a Command, &'b [String])> {
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

pub(super) fn append_help(
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

#[cfg(test)]
mod tests {
    use super::{Command, Handler, append_help, commands};

    #[test]
    fn user_requirement_matches_every_handler() {
        fn check(commands: &[Command]) {
            for command in commands {
                match command.handle {
                    Some(Handler::User(_)) => assert!(!command.global, "{}", command.name),
                    Some(Handler::Global(_)) => assert!(command.global, "{}", command.name),
                    None => assert!(!command.children.is_empty()),
                }
                check(&command.children);
            }
        }
        check(commands());
    }

    #[test]
    fn help_transcript_preserves_user_and_admin_scope() {
        let mut lines = Vec::new();
        append_help(commands(), &[], false, false, &mut lines);
        assert_eq!(
            lines,
            [
                "certfp fingerprint",
                "certfp generate",
                "channel create",
                "channel delete",
                "channel status",
                "channel update",
                "device-certificate create",
                "device-certificate delete",
                "device-certificate status",
                "help",
                "network create",
                "network delete",
                "network info",
                "network quote",
                "network status",
                "network update",
                "sasl reset",
                "sasl set-plain",
                "sasl status",
                "user delete",
                "user update",
            ]
        );
        lines.clear();
        append_help(commands(), &[], true, true, &mut lines);
        assert_eq!(
            lines,
            [
                "help",
                "server debug",
                "server notice",
                "server status",
                "user create",
                "user delete",
                "user run",
                "user status",
                "user update"
            ]
        );
    }
}
