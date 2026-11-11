//! Administrative IRC connections used by sojuctl.

use super::Server;
use crate::conn::{Conn, ConnReader};
use crate::irc::numerics::*;
use crate::logger::Logger;
use crate::{logf, msg};
use anyhow::anyhow;
use std::sync::Arc;

impl Server {
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
