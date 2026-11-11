//! Cancellation-safe reads and bounded input permits.

use super::{ConnReader, InputPermit};
use crate::debugf;
use crate::irc::Message;
use anyhow::Result;

impl ConnReader {
    /// Reads an incoming message. Ok(None) is returned when there are no
    /// more messages (EOF or the connection was closed).
    pub async fn read_message(&mut self) -> Result<Option<Message>> {
        let res = tokio::select! {
            res = self.reader.read_raw(&mut self.line_buf) => res,
            _ = self.cancel.cancelled() => return Ok(None),
        };
        if let Ok(Some(msg)) = &res {
            debugf!(
                self.logger,
                "received: {}",
                crate::ircutil::redact_for_log(msg)
            );
        }
        res
    }

    /// Like read_message, but the message is returned once less than
    /// MAX_INPUT_IN_FLIGHT messages previously returned by this function
    /// still have their permit alive. The permit is meant to be dropped once
    /// the message is processed. PING and PONG use a separate small pool:
    /// the connection stays alive while its other input waits without
    /// allowing an unbounded keepalive backlog. Cancel-safe.
    pub async fn read_message_limited(&mut self) -> Result<Option<(Message, InputPermit)>> {
        if self.pending.is_none() {
            match self.read_message().await? {
                Some(msg) => self.pending = Some(msg),
                None => return Ok(None),
            }
        }
        let keepalive = self
            .pending
            .as_ref()
            .is_some_and(|m| m.command == "PING" || m.command == "PONG");
        let input = if keepalive {
            &self.keepalive_input
        } else {
            &self.input
        };
        let permit = tokio::select! {
            p = input.clone().acquire_owned() => Some(p.expect("semaphore is never closed")),
            _ = self.cancel.cancelled() => return Ok(None),
        };
        Ok(self.pending.take().map(|msg| (msg, permit)))
    }
}
