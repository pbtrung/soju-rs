//! METADATA from registered downstream connections.

use super::*;

pub(super) fn handle_metadata(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 2)?;
    let target = msg.params[0].clone();
    check_target("METADATA", &target)?;
    let subcommand = msg.params[1].clone();
    let net_idx = dc_network(u, i);
    let net_id = net_idx.map(|ni| u.networks[ni].id());

    let srv = u.srv.clone();
    let list =
        move || -> Result<Vec<db::MessageTarget>> { srv.db.list_message_targets(net_id.unwrap()) };
    let targets_fn: Option<&dyn Fn() -> Result<Vec<db::MessageTarget>>> =
        if net_id.is_some() { Some(&list) } else { None };
    if let Some(res) = handle_metadata_sub(&mut u.downstreams[i], msg, targets_fn) {
        return res;
    }

    let mut mt: Option<db::MessageTarget> = None;
    if let Some(ni) = net_idx
        && target != "*"
    {
        let target_cm = u.networks[ni].casemap(&target);
        match u.srv.db.get_message_target(u.networks[ni].id(), &target_cm) {
            Ok(m) => mt = Some(m),
            Err(e) => {
                logf!(
                    dc!(u, i).logger,
                    "failed to get the message target for {target:?}: {e}"
                );
                return Err(fail(&[
                    "METADATA",
                    "INTERNAL_ERROR",
                    &target,
                    "Internal error",
                ]));
            }
        }
    }

    match subcommand.as_str() {
        "LIST" | "GET" => {
            let m = mt
                .as_ref()
                .map(get_message_target_metadata)
                .unwrap_or_default();
            let mut msgs = Vec::new();
            if subcommand == "LIST" {
                for (k, v) in &m {
                    msgs.push(msg!(RPL_KEYVALUE, "*", target.clone(), *k, "*", *v));
                }
            } else {
                for k in &msg.params[2..] {
                    let k = k.to_lowercase();
                    match m.iter().find(|(key, _)| *key == k) {
                        Some((_, v)) => {
                            msgs.push(msg!(RPL_KEYVALUE, "*", target.clone(), k, "*", *v))
                        }
                        None => {
                            msgs.push(msg!("FAIL", "METADATA", "KEY_INVALID", k, "Invalid key"))
                        }
                    }
                }
            }
            dc!(u, i).send_batch("metadata", &[], msgs);
        }
        "SET" | "CLEAR" => {
            let changes: Vec<(String, Option<String>)> = if subcommand == "SET" {
                need_params(msg, 3)?;
                vec![(msg.params[2].to_lowercase(), msg.params.get(3).cloned())]
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
            if let (Some(mut mt), Some(net_id)) = (mt, net_id)
                && let Err(e) = u.srv.db.store_message_target(net_id, &mut mt)
            {
                logf!(
                    dc!(u, i).logger,
                    "failed to store the message target for {target:?}: {e}"
                );
                return Err(fail(&[
                    "METADATA",
                    "INTERNAL_ERROR",
                    &target,
                    "Internal error",
                ]));
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

fn parse_metadata_bool(v: &Option<String>) -> Option<(bool, &'static str)> {
    match v.as_deref() {
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
        let Some((b, mv)) = parse_metadata_bool(v) else {
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
