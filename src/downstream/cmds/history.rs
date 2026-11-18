//! CHATHISTORY, READ/MARKREAD and SEARCH from registered downstream
//! connections.

use super::{
    ERR_UNKNOWNCOMMAND, LoadMessageOptions, Message, Result, SERVICE_NICK_CM, SearchMessageOptions,
    Tags, Timestamp, User, anyhow, chathistory_error, db, dc_network, dcs_for, fail,
    internal_error, irc_error, ircutil, logf, msg, need_params, xirc,
};
use crate::config::MAX_CHAT_HISTORY_LIMIT;

/// History replies are refused while a client has this many messages it
/// hasn't read yet, so that a client which stops reading can't make us queue
/// replies without bounds.
const MAX_QUEUED_FOR_HISTORY: usize = 10 * MAX_CHAT_HISTORY_LIMIT;
const SEARCH_MAX_LIMIT: usize = 100;

fn check_queue(u: &User, i: usize, cmd: &str) -> Result<()> {
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        if u.downstreams[i].conn.queued() > MAX_QUEUED_FOR_HISTORY {
            return Err(fail(&[
                cmd,
                "TEMPORARILY_UNAVAILABLE",
                "Too many messages waiting to be sent, try again later",
            ]));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryCommand {
    Before,
    After,
    Around,
    Latest,
    Between,
    Targets,
}

impl HistoryCommand {
    fn name(self) -> &'static str {
        match self {
            Self::Before => "BEFORE",
            Self::After => "AFTER",
            Self::Around => "AROUND",
            Self::Latest => "LATEST",
            Self::Between => "BETWEEN",
            Self::Targets => "TARGETS",
        }
    }
}

/// Wire fields are separated from validation so service/bouncer shortcuts keep
/// their existing precedence over invalid bounds and limits.
struct HistoryFields<'a> {
    command: HistoryCommand,
    target: &'a str,
    first: &'a str,
    second: &'a str,
    limit: &'a str,
}

impl<'a> HistoryFields<'a> {
    fn parse(msg: &'a Message) -> Result<Self> {
        // The entry point checked the subcommand before queue admission.
        let name = msg.param(0);
        let command = match name {
            "BEFORE" => HistoryCommand::Before,
            "AFTER" => HistoryCommand::After,
            "AROUND" => HistoryCommand::Around,
            "LATEST" => HistoryCommand::Latest,
            "BETWEEN" => HistoryCommand::Between,
            "TARGETS" => HistoryCommand::Targets,
            _ => {
                return Err(fail(&[
                    "CHATHISTORY",
                    "INVALID_PARAMS",
                    name,
                    "Unknown command",
                ]));
            }
        };
        need_params(
            msg,
            if command == HistoryCommand::Between {
                5
            } else {
                4
            },
        )?;
        let (target, first, second, limit) = match command {
            HistoryCommand::Targets => ("", msg.param(1), msg.param(2), msg.param(3)),
            HistoryCommand::Between => (msg.param(1), msg.param(2), msg.param(3), msg.param(4)),
            _ => (msg.param(1), msg.param(2), "", msg.param(3)),
        };
        Ok(Self {
            command,
            target,
            first,
            second,
            limit,
        })
    }

    fn validate(&self, configured_limit: usize) -> Result<HistoryRequest> {
        let name = self.command.name();
        let first = if self.command == HistoryCommand::Latest && self.first == "*" {
            None
        } else {
            Some(ircutil::parse_chathistory_bound(self.first).ok_or_else(|| {
                fail(&[
                    "CHATHISTORY",
                    "INVALID_PARAMS",
                    name,
                    self.first,
                    "Invalid first bound",
                ])
            })?)
        };
        let second = if self.second.is_empty() {
            None
        } else {
            Some(
                ircutil::parse_chathistory_bound(self.second).ok_or_else(|| {
                    fail(&[
                        "CHATHISTORY",
                        "INVALID_PARAMS",
                        name,
                        self.second,
                        "Invalid second bound",
                    ])
                })?,
            )
        };
        let limit = match self.limit.parse::<i64>() {
            Ok(limit) if (0..=i64::try_from(MAX_CHAT_HISTORY_LIMIT)?).contains(&limit) => {
                usize::try_from(limit)?
            }
            _ => {
                return Err(fail(&[
                    "CHATHISTORY",
                    "INVALID_PARAMS",
                    name,
                    self.limit,
                    "Invalid limit",
                ]));
            }
        };
        let query = match (self.command, first, second) {
            (HistoryCommand::Before, Some(bound), _) => HistoryQuery::Before(bound),
            (HistoryCommand::After, Some(bound), _) => HistoryQuery::After(bound),
            (HistoryCommand::Around, Some(bound), _) => HistoryQuery::Around(bound),
            (HistoryCommand::Latest, bound, _) => HistoryQuery::Latest(bound),
            (HistoryCommand::Between, Some(first), second) => HistoryQuery::Between(first, second),
            (HistoryCommand::Targets, Some(start), Some(end)) => {
                return Ok(HistoryRequest::Targets { start, end, limit });
            }
            _ => {
                return Err(fail(&[
                    "CHATHISTORY",
                    "INVALID_PARAMS",
                    name,
                    "Invalid bounds",
                ]));
            }
        };
        Ok(HistoryRequest::Messages {
            query,
            limit: limit.min(configured_limit),
        })
    }
}

#[derive(Debug)]
enum HistoryQuery {
    Before(Timestamp),
    After(Timestamp),
    Around(Timestamp),
    Latest(Option<Timestamp>),
    Between(Timestamp, Option<Timestamp>),
}

enum HistoryRequest {
    Messages {
        query: HistoryQuery,
        limit: usize,
    },
    Targets {
        start: Timestamp,
        end: Timestamp,
        limit: usize,
    },
}

pub(super) fn handle_chathistory(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 1)?;
    check_queue(u, i, "CHATHISTORY")?;
    // TARGETS on an unbound bouncer connection has always returned an empty
    // batch, even when the remaining parameters are missing.
    if msg.param(0) == "TARGETS" && dc_network(u, i).is_none() {
        dc!(u, i).send_batch("draft/chathistory-targets", &[], Vec::new());
        return Ok(());
    }
    let fields = HistoryFields::parse(msg)?;
    if dc!(u, i).casemap(fields.target) == SERVICE_NICK_CM {
        dc!(u, i).send_batch("chathistory", &[fields.target.to_owned()], Vec::new());
        return Ok(());
    }
    if !u.has_chathistory_store() {
        let nick = dc!(u, i).nick.clone();
        return Err(irc_error(msg!(
            ERR_UNKNOWNCOMMAND,
            nick,
            "CHATHISTORY",
            "Chat history disabled"
        )));
    }
    let Some(ni) = dc_network(u, i) else {
        return Err(fail(&[
            "CHATHISTORY",
            "INVALID_TARGET",
            fields.command.name(),
            fields.target,
            "Cannot fetch chat history on bouncer connection",
        ]));
    };
    let request = fields.validate(u.srv.config().basic.chat_history_limit)?;
    let net = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow!("history network is unavailable"))?;
    let target = net.casemap(fields.target);
    let events = dc!(u, i).caps.is_enabled("draft/event-playback");
    let net = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow!("history network is unavailable"))?;
    match request {
        HistoryRequest::Messages { query, limit } => {
            let options = LoadMessageOptions {
                network: &net.record,
                entity: &target,
                limit,
                events,
            };
            let history = query.execute(&mut u.msg_store, &options).map_err(|error| {
                logf!(
                    dc!(u, i).logger,
                    "failed fetching {target:?} messages for chathistory: {error}"
                );
                chathistory_error(fields.command.name(), &target)
            })?;
            dc!(u, i).send_batch("chathistory", &[target], history);
        }
        HistoryRequest::Targets { start, end, limit } => {
            let targets = u
                .msg_store
                .list_targets(&net.record, start, end, limit, events)
                .map_err(|error| {
                    logf!(
                        dc!(u, i).logger,
                        "failed fetching targets for chathistory: {error}"
                    );
                    fail(&[
                        "CHATHISTORY",
                        "MESSAGE_ERROR",
                        fields.command.name(),
                        "Failed to retrieve targets",
                    ])
                })?;
            send_history_targets(u, i, ni, targets)?;
        }
    }
    Ok(())
}

fn send_history_targets(
    u: &mut User,
    i: usize,
    ni: usize,
    targets: Vec<crate::msgstore::ChatHistoryTarget>,
) -> Result<()> {
    let unread_tags = dc!(u, i).caps.is_enabled("soju.im/unread");
    let net = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow!("history network is unavailable"))?;
    let messages: Vec<_> = targets
        .into_iter()
        .filter(|target| {
            !net.channels
                .get(&target.name)
                .is_some_and(|channel| channel.detached)
        })
        .map(|target| {
            let mut message = msg!(
                "CHATHISTORY",
                "TARGETS",
                target.name,
                xirc::format_server_time(target.latest_message)
            );
            if unread_tags && target.unread {
                message.tags.set("soju.im/unread", "");
            }
            if unread_tags && target.mention {
                message.tags.set("soju.im/mention", "");
            }
            message
        })
        .collect();
    dc!(u, i).send_batch("draft/chathistory-targets", &[], messages);
    Ok(())
}

impl HistoryQuery {
    fn execute(
        &self,
        store: &mut crate::msgstore::Store,
        options: &LoadMessageOptions<'_>,
    ) -> Result<Vec<Message>> {
        // No upper bound rather than now: if the upstream server's clock is
        // ahead of ours, its latest messages are in our future
        match *self {
            Self::Before(bound) => store.load_before_time(Some(bound), None, options),
            Self::Latest(bound) => store.load_before_time(None, bound, options),
            Self::After(bound) => store.load_after_time(Some(bound), None, options),
            Self::Around(bound) => (|| {
                let after_limit = options.limit / 2;
                let before_limit = options.limit - after_limit;
                let before = store.load_before_time(
                    Some(bound),
                    None,
                    &LoadMessageOptions {
                        limit: before_limit,
                        ..*options
                    },
                )?;
                let after_bound = match before.last() {
                    Some(last) => xirc::parse_server_time(last.tags.get_or_empty("time"))
                        .ok_or_else(|| {
                            anyhow!(
                                "parsing time {:?}: cannot parse",
                                last.tags.get_or_empty("time")
                            )
                        })?,
                    // There are no messages before, subtract an arbitrary amount
                    // of time to make the bound inclusive
                    None => bound - jiff::SignedDuration::from_secs(1),
                };
                let after = store.load_after_time(
                    Some(after_bound),
                    None,
                    &LoadMessageOptions {
                        limit: after_limit,
                        ..*options
                    },
                )?;
                let mut h = before;
                h.extend(after);
                Ok(h)
            })(),
            Self::Between(first, second) => {
                if Some(first) < second {
                    store.load_after_time(Some(first), second, options)
                } else {
                    store.load_before_time(Some(first), second, options)
                }
            }
        }
    }
}

/// Rejects a target which can't be a middle parameter of our replies: it
/// would corrupt them, e.g. "a b" would be read as two parameters.
pub(super) fn check_target(cmd: &str, target: &str) -> Result<()> {
    if target.is_empty() || target.contains(' ') || target.starts_with(':') {
        return Err(fail(&[cmd, "INVALID_TARGET", "*", "Invalid target"]));
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
pub(super) fn handle_read(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if msg.params.is_empty() {
        return Err(fail(&[
            &msg.command,
            "NEED_MORE_PARAMS",
            "Missing parameters",
        ]));
    }
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let target = msg.params[0].clone();
    check_target(&msg.command, &target)?;
    let criteria = msg.params.get(1).cloned().unwrap_or_default();

    // We don't save read receipts for our service
    if dc!(u, i).casemap(&target) == SERVICE_NICK_CM {
        let prefix = dc!(u, i).prefix();
        dc!(u, i).send_message(msg!(msg.command.clone(), target, "*").with_prefix(prefix));
        return Ok(());
    }

    let Some(ni) = dc_network(u, i) else {
        return Err(fail(&[
            &msg.command,
            "INTERNAL_ERROR",
            &target,
            "Cannot set read markers on bouncer connection",
        ]));
    };
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net_id = u.networks[ni].id();
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let target_cm = u.networks[ni].casemap(&target);

    let mut r = match u.srv.db.get_read_receipt(net_id, &target_cm) {
        Ok(Some(r)) => r,
        Ok(None) => db::ReadReceipt {
            target: target_cm.clone(),
            ..Default::default()
        },
        Err(e) => {
            logf!(
                dc!(u, i).logger,
                "failed to get the read receipt for {target:?}: {e}"
            );
            return Err(internal_error(&msg.command, &target));
        }
    };

    let mut broadcast = false;
    if !criteria.is_empty() {
        // TODO: support msgid criteria
        let Some(("timestamp", ts)) = criteria.split_once('=') else {
            return Err(fail(&[
                &msg.command,
                "INVALID_PARAMS",
                &criteria,
                "Unknown criteria",
            ]));
        };
        let Some(mut timestamp) = xirc::parse_server_time(ts) else {
            return Err(fail(&[
                &msg.command,
                "INVALID_PARAMS",
                &criteria,
                "Invalid criteria",
            ]));
        };
        let now = Timestamp::now();
        if r.timestamp.is_none_or(|t| t < timestamp) && timestamp > now {
            // Only allow read markers after now if the upstream server's
            // clock is ahead of ours, i.e. if we have a message more recent
            // than now in history
            let mut limit = now;
            if u.msg_store.supports_chathistory() {
                let store = &mut u.msg_store;
                #[expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                let options = LoadMessageOptions {
                    network: &u.networks[ni].record,
                    entity: &target_cm,
                    limit: 1,
                    events: true,
                };
                let latest = store.load_before_time(None, None, &options).and_then(|h| {
                    h.first()
                        .map(|m| {
                            let t = m.tags.get_or_empty("time");
                            xirc::parse_server_time(t)
                                .ok_or_else(|| anyhow!("parsing time {t:?}: cannot parse"))
                        })
                        .transpose()
                });
                match latest {
                    Ok(Some(latest)) if latest > limit => limit = latest,
                    Ok(_) => {}
                    Err(e) => {
                        logf!(
                            dc!(u, i).logger,
                            "failed to get the latest message time for {target:?}: {e}"
                        );
                        return Err(internal_error(&msg.command, &target));
                    }
                }
            }
            timestamp = timestamp.min(limit);
        }
        if r.timestamp.is_none_or(|t| t < timestamp) {
            r.timestamp = Some(timestamp);
            if let Err(e) = u.srv.db.store_read_receipt(net_id, &mut r) {
                logf!(
                    dc!(u, i).logger,
                    "failed to store receipt for {target:?}: {e}"
                );
                return Err(internal_error(&msg.command, &target));
            }
            broadcast = true;
        }
    }

    let timestamp_str = match r.timestamp {
        Some(t) => format!("timestamp={}", xirc::format_server_time(t)),
        None => "*".to_owned(),
    };
    let dc_id = dc!(u, i).id;
    for d in dcs_for(&mut u.downstreams, net_id) {
        if broadcast || d.id == dc_id {
            let cmd = if d.caps.is_enabled("draft/read-marker") {
                "MARKREAD"
            } else {
                "READ"
            };
            let prefix = d.prefix();
            d.send_message(msg!(cmd, target.clone(), timestamp_str.clone()).with_prefix(prefix));
        }
    }

    let mut push = false;
    {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let mut pt = crate::state_lock::lock(&u.networks[ni].push_targets);
        if broadcast && pt.has(&target) {
            // TODO: only broadcast if draft/read-marker has been negotiated
            if let (Some(rt), Some(&t)) = (r.timestamp, pt.get(&target))
                && rt >= t
            {
                pt.del(&target);
            }
            push = true;
        }
    }
    if push {
        u.broadcast_web_push(ni, msg!("MARKREAD", target, timestamp_str));
    }
    Ok(())
}

pub(super) fn handle_search(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    check_queue(u, i, "SEARCH")?;
    if !u.msg_store.supports_search() {
        let nick = dc!(u, i).nick.clone();
        return Err(irc_error(msg!(
            ERR_UNKNOWNCOMMAND,
            nick,
            "SEARCH",
            "Unknown command"
        )));
    }
    need_params(msg, 1)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let attrs = Tags::parse(&msg.params[0]);

    let mut network = None;
    let mut opts = SearchMessageOptions {
        limit: SEARCH_MAX_LIMIT,
        ..Default::default()
    };
    for (name, value) in attrs.iter() {
        match name {
            "before" | "after" => {
                let Some(t) = xirc::parse_server_time(value) else {
                    return Err(fail(&[
                        "SEARCH",
                        "INVALID_PARAMS",
                        name,
                        "Invalid criteria",
                    ]));
                };
                if name == "after" {
                    opts.start = Some(t);
                } else {
                    opts.end = Some(t);
                }
            }
            "from" => value.clone_into(&mut opts.from),
            "in" => {
                let Some(ni) = dc_network(u, i) else {
                    return Err(fail(&[
                        "SEARCH",
                        "INVALID_PARAMS",
                        name,
                        "Cannot search on bouncer connection",
                    ]));
                };
                network = Some(ni);
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    opts.r#in = u.networks[ni].casemap(value);
                }
            }
            "text" => value.clone_into(&mut opts.text),
            "limit" => match value.parse::<i64>() {
                Ok(l) if l > 0 => opts.limit = usize::try_from(l)?,
                _ => return Err(fail(&["SEARCH", "INVALID_PARAMS", name, "Invalid limit"])),
            },
            _ => {}
        }
    }
    let Some(ni) = network else {
        return Err(fail(&[
            "SEARCH",
            "INVALID_PARAMS",
            "in",
            "The in parameter is mandatory",
        ]));
    };
    if opts.limit > SEARCH_MAX_LIMIT {
        opts.limit = SEARCH_MAX_LIMIT;
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let messages = match u.msg_store.search(&u.networks[ni].record, &opts) {
        Ok(m) => m,
        Err(e) => {
            logf!(dc!(u, i).logger, "failed fetching messages for search: {e}");
            return Err(fail(&[
                "SEARCH",
                "INTERNAL_ERROR",
                "Messages could not be retrieved",
            ]));
        }
    };
    dc!(u, i).send_batch("soju.im/search", &[], messages);
    Ok(())
}
