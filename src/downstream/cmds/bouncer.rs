//! BOUNCER (soju.im/bouncer-networks) from registered downstream
//! connections.

use super::{
    MULTIPLEX_CAP, Message, Result, Tags, User, attach_network, db, detach_network, fail,
    find_attached, msg, need_params, parse_bouncer_net_id, update_network_attrs,
};

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
pub(super) fn handle_bouncer(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u
        .dc_index(dc_id)
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    need_params(msg, 1)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let subcommand = msg.params[0].clone();
    match subcommand.to_uppercase().as_str() {
        "BIND" => {
            return Err(fail(&[
                "BOUNCER",
                "REGISTRATION_IS_COMPLETED",
                "BIND",
                "Cannot bind to a network after registration",
            ]));
        }
        "LISTNETWORKS" => crate::downstream::send_bouncer_networks(u, i),
        "ADDNETWORK" => {
            need_params(msg, 2)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let attrs = Tags::parse(&msg.params[1]);
            let mut record = db::Network::new("");
            record.nick.clone_from(&dc!(u, i).nick);
            update_network_attrs(&mut record, &attrs, &subcommand)?;
            if record.nick == u.record.username {
                record.nick.clear();
            }
            if record.realname == u.record.realname {
                record.realname.clear();
            }
            let ni = match u.create_network(record, true) {
                Ok(ni) => ni,
                Err(e) => {
                    return Err(fail(&[
                        "BOUNCER",
                        "UNKNOWN_ERROR",
                        &subcommand,
                        &format!("Failed to create network: {e}"),
                    ]));
                }
            };
            #[expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            let id = u.networks[ni].id();
            let i = u.dc_index(dc_id).ok_or_else(|| {
                anyhow::anyhow!("required user or connection state is unavailable")
            })?;
            dc!(u, i).send_message(msg!("BOUNCER", "ADDNETWORK", id.to_string()));
        }
        "CHANGENETWORK" => {
            need_params(msg, 3)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let id_str = msg.params[1].clone();
            let id = parse_bouncer_net_id(&subcommand, &id_str)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let attrs = Tags::parse(&msg.params[2]);
            let Some(ni) = u.net_index(id) else {
                return Err(fail(&[
                    "BOUNCER",
                    "INVALID_NETID",
                    &subcommand,
                    &id_str,
                    "Invalid network ID",
                ]));
            };
            #[expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            let mut record = u.networks[ni].record.clone();
            let was_enabled = record.enabled;
            update_network_attrs(&mut record, &attrs, &subcommand)?;
            if record.nick == u.record.username {
                record.nick.clear();
            }
            if record.realname == u.record.realname {
                record.realname.clear();
            }
            if let Err(e) = u.update_network(record, !was_enabled) {
                return Err(fail(&[
                    "BOUNCER",
                    "UNKNOWN_ERROR",
                    &subcommand,
                    &format!("Failed to update network: {e}"),
                ]));
            }
            if let Some(i) = u.dc_index(dc_id) {
                dc!(u, i).send_message(msg!("BOUNCER", "CHANGENETWORK", id_str));
            }
        }
        "DELNETWORK" => {
            need_params(msg, 2)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let id_str = msg.params[1].clone();
            let id = parse_bouncer_net_id(&subcommand, &id_str)?;
            if u.net_index(id).is_none() {
                return Err(fail(&[
                    "BOUNCER",
                    "INVALID_NETID",
                    &subcommand,
                    &id_str,
                    "Invalid network ID",
                ]));
            }
            u.delete_network(id)?;
            if let Some(i) = u.dc_index(dc_id) {
                dc!(u, i).send_message(msg!("BOUNCER", "DELNETWORK", id_str));
            }
        }
        "ATTACH" | "DETACH" if dc!(u, i).caps.is_enabled(MULTIPLEX_CAP) => {
            need_params(msg, 2)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let id_str = msg.params[1].clone();
            let id = parse_bouncer_net_id(&subcommand, &id_str)?;
            if dc!(u, i).network_id.is_some() || dc!(u, i).parent_id.is_some() {
                return Err(fail(&[
                    "BOUNCER",
                    "INVALID_CONNECTION",
                    &subcommand,
                    "Networks can only be attached to the bouncer connection",
                ]));
            }
            if subcommand.eq_ignore_ascii_case("DETACH") {
                // Detaching a network which isn't attached is a no-op: the
                // bouncer may have closed it meanwhile
                if let Some(child) = find_attached(u, dc_id, id) {
                    detach_network(u, child);
                }
                return Ok(());
            }
            if u.net_index(id).is_none() {
                return Err(fail(&[
                    "BOUNCER",
                    "INVALID_NETID",
                    &subcommand,
                    &id_str,
                    "Invalid network ID",
                ]));
            }
            attach_network(u, dc_id, id);
        }
        _ => {
            return Err(fail(&[
                "BOUNCER",
                "UNKNOWN_COMMAND",
                &subcommand,
                "Unknown subcommand",
            ]));
        }
    }
    Ok(())
}
