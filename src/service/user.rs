//! BouncerServ user commands.

use anyhow::{Result, anyhow, bail};
use sha1::Sha1;
use sha2::Digest;
use tokio::sync::mpsc;

use super::{
    ServiceContext, handle_service_command, hash_password, no_args, parse_flags, pop_arg,
    run_on_user, wait,
};
use crate::database as db;
use crate::flags::FlagSet;
use crate::logf;
use crate::user::{Event, User, UserRunMsg};

pub(super) fn handle_user_status(
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

pub(super) fn handle_user_create(
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
    let srv = ctx.srv.clone();
    // Hashing the password takes a while
    ctx.defer(move |mut print| async move {
        if !disable_password {
            user.password = hash_password(password).await?;
        }
        srv.create_user(user)
            .map_err(|e| anyhow!("could not create user: {e}"))?;
        print(format!("created user {username:?}"));
        Ok(())
    });
    Ok(())
}

pub(super) fn handle_user_update(
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
        let Some(handle) = ctx.srv.get_user(&username) else {
            bail!("unknown username {username:?}");
        };
        let deadline = ctx.deadline;
        ctx.defer(move |mut print| async move {
            let hashed = match password {
                Some(pw) => Some(hash_password(pw).await?),
                None if disable_password => Some(String::new()),
                None => None,
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
            wait(deadline, rx)
                .await
                .ok_or_else(|| anyhow!("context deadline exceeded"))?
                .map_err(|_| anyhow!("timeout executing command"))?
                .map_err(|e| anyhow!(e))?;
            print(format!("updated user {username:?}"));
            Ok(())
        });
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
        let apply = move |u: &mut User, hashed: Option<String>| {
            u.update_user(|record| {
                if let Some(h) = hashed {
                    record.password = h;
                }
                if disable_password {
                    record.password.clear();
                }
                if let Some(n) = nick {
                    record.nick = n;
                }
                if let Some(r) = realname {
                    record.realname = r;
                }
                if let Some(m) = max_networks {
                    record.max_networks = m;
                }
                Ok(())
            })
        };
        let name = u.record.username.clone();
        match password {
            None => {
                apply(u, None)?;
                ctx.print(format!("updated user {name:?}"));
            }
            // Hashing the password takes a while
            Some(pw) => {
                let tx = u.tx.clone();
                ctx.defer(move |mut print| async move {
                    let hashed = hash_password(pw).await?;
                    run_on_user(&tx, move |u| apply(u, Some(hashed))).await?;
                    print(format!("updated user {name:?}"));
                    Ok(())
                });
            }
        }
    }
    Ok(())
}

pub(super) fn handle_user_delete(
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

    let deadline = ctx.deadline;
    let srv = ctx.srv.clone();
    ctx.defer(move |mut print| async move {
        if wait(deadline, handle.stop()).await.is_none() {
            bail!("failed to stop user: context deadline exceeded");
        }
        db::run(&srv.db, move |db| db.delete_user(user_id))
            .await
            .map_err(|e| anyhow!("failed to delete user: {e}"))?;
        print(format!("deleted user {username:?}"));
        Ok(())
    });
    Ok(())
}

pub(super) fn handle_user_run(
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
    let deadline = ctx.deadline;
    ctx.defer(move |mut print| async move {
        loop {
            match wait(deadline, rx.recv()).await.flatten() {
                Some(UserRunMsg::Message(m)) => print(m),
                Some(UserRunMsg::Done(res)) => return res.map_err(|e| anyhow!(e)),
                None => bail!("timeout executing command"),
            }
        }
    });
    Ok(())
}
