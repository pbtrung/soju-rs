//! BOUNCER (soju.im/bouncer-networks) from registered downstream
//! connections.

use super::*;

pub(super) fn handle_bouncer(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    need_params(msg, 1)?;
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
            let attrs = Tags::parse(&msg.params[1]);
            let mut record = db::Network::new("");
            record.nick = dc!(u, i).nick.clone();
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
            let id = u.networks[ni].id();
            let i = u.dc_index(dc_id).unwrap();
            dc!(u, i).send_message(msg!("BOUNCER", "ADDNETWORK", id.to_string()));
        }
        "CHANGENETWORK" => {
            need_params(msg, 3)?;
            let id_str = msg.params[1].clone();
            let id = parse_bouncer_net_id(&subcommand, &id_str)?;
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
