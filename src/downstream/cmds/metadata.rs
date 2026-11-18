//! METADATA from registered downstream connections.

use super::{
    Message, RPL_KEYVALUE, Result, Tags, User, check_target, db, dc_network, dcs_for, fail,
    get_message_target_metadata, handle_metadata_sub, internal_error, logf, msg, need_params,
};

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
#[expect(
    clippy::many_single_char_names,
    reason = "IRC dispatch coordinates u/i and short metadata key/value loop bindings follow the surrounding protocol code."
)]
pub(super) fn handle_metadata(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 2)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let target = msg.params[0].clone();
    check_target("METADATA", &target)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let subcommand = msg.params[1].clone();
    let net_idx = dc_network(u, i);
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net_id = net_idx.map(|ni| u.networks[ni].id());

    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let flagged = net_idx.map(|ni| u.networks[ni].flagged_targets());
    let list =
        move || -> Result<Vec<db::MessageTarget>> { Ok(flagged.clone().unwrap_or_default()) };
    let targets_fn: Option<&dyn Fn() -> Result<Vec<db::MessageTarget>>> =
        if net_id.is_some() { Some(&list) } else { None };
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        if let Some(res) = handle_metadata_sub(&mut u.downstreams[i], msg, targets_fn) {
            return res;
        }
    }

    let mut mt: Option<db::MessageTarget> = None;
    if let Some(ni) = net_idx
        && target != "*"
    {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let target_cm = u.networks[ni].casemap(&target);
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            mt = Some(u.networks[ni].message_target(&target_cm));
        }
    }

    match subcommand.as_str() {
        "LIST" | "GET" => {
            let m = mt.as_ref().map_or_default(get_message_target_metadata);
            let mut msgs = Vec::new();
            if subcommand == "LIST" {
                for (k, v) in &m {
                    msgs.push(msg!(RPL_KEYVALUE, "*", target.clone(), *k, "*", *v));
                }
            } else {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                    )]
                    for k in &msg.params[2..] {
                        let k = k.to_lowercase();
                        match m.iter().find(|(key, _)| *key == k) {
                            Some((_, v)) => {
                                msgs.push(msg!(RPL_KEYVALUE, "*", target.clone(), k, "*", *v));
                            }
                            None => {
                                msgs.push(msg!(
                                    "FAIL",
                                    "METADATA",
                                    "KEY_INVALID",
                                    k,
                                    "Invalid key"
                                ));
                            }
                        }
                    }
                }
            }
            dc!(u, i).send_batch("metadata", &[], msgs);
        }
        "SET" | "CLEAR" => {
            let changes: Vec<(String, Option<String>)> = if subcommand == "SET" {
                need_params(msg, 3)?;
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                    )]
                    vec![(msg.params[2].to_lowercase(), msg.params.get(3).cloned())]
                }
            } else {
                vec![
                    ("soju.im/pinned".to_owned(), None),
                    ("soju.im/muted".to_owned(), None),
                    ("soju.im/blocked".to_owned(), None),
                ]
            };
            let batch = if subcommand == "CLEAR" {
                Some(dc!(u, i).begin_batch("metadata", &[], Tags::new()))
            } else {
                None
            };
            set_message_target_metadata(u, i, &target, mt.as_mut(), &changes, batch.as_deref());
            if let Some(b) = batch {
                dc!(u, i).end_batch(&b);
            }
            if let (Some(mut mt), Some(ni)) = (mt, net_idx) {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    if let Err(e) = u.srv.db.store_message_target(u.networks[ni].id(), &mut mt) {
                        logf!(
                            dc!(u, i).logger,
                            "failed to store the message target for {target:?}: {e}"
                        );
                        return Err(internal_error("METADATA", &target));
                    }
                }
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    u.networks[ni].set_message_target(mt);
                }
            }
        }
        _ => {
            // TODO: support SYNC
            return Err(fail(&[
                "METADATA",
                "INVALID_PARAMS",
                &subcommand,
                "Unknown command",
            ]));
        }
    }
    Ok(())
}

fn parse_metadata_bool(v: Option<&String>) -> Option<(bool, &'static str)> {
    match v.map(String::as_str) {
        None | Some("0") => Some((false, "0")),
        Some("1") => Some((true, "1")),
        _ => None,
    }
}

fn set_message_target_metadata(
    u: &mut User,
    i: usize,
    target: &str,
    mut mt: Option<&mut db::MessageTarget>,
    changes: &[(String, Option<String>)],
    batch: Option<&str>,
) {
    let tags = match batch {
        Some(b) => Tags::from_iter([("batch", b)]),
        None => Tags::new(),
    };
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net_id = u.downstreams[i].network_id;
    for (k, v) in changes {
        let Some(mt) = mt.as_deref_mut() else {
            dc!(u, i).send_message(
                msg!("FAIL", "METADATA", "KEY_INVALID", k.clone(), "Invalid key")
                    .with_tags(tags.clone()),
            );
            continue;
        };
        let field = match k.as_str() {
            "soju.im/pinned" => &mut mt.pinned,
            "soju.im/muted" => &mut mt.muted,
            "soju.im/blocked" => &mut mt.blocked,
            _ => {
                dc!(u, i).send_message(
                    msg!("FAIL", "METADATA", "KEY_INVALID", k.clone(), "Invalid key")
                        .with_tags(tags.clone()),
                );
                continue;
            }
        };
        let Some((b, mv)) = parse_metadata_bool(v.as_ref()) else {
            dc!(u, i).send_message(
                msg!("FAIL", "METADATA", "VALUE_INVALID", "Invalid value").with_tags(tags.clone()),
            );
            continue;
        };
        *field = b;
        dc!(u, i).send_message(
            msg!(RPL_KEYVALUE, "*", target, k.clone(), "*", mv).with_tags(tags.clone()),
        );
        if let Some(net_id) = net_id {
            for d in dcs_for(&mut u.downstreams, net_id) {
                d.send_message(msg!("METADATA", target, k.clone(), "*", mv));
            }
        }
    }
}
