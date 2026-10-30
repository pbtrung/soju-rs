//! soju.im/multiplex: the bouncer networks of a client on its bouncer
//! connection, instead of one connection per network. See
//! doc/multiplex.md.
//!
//! Each attached network is a virtual downstream connection: a
//! DownstreamConn whose Conn is a view of the bouncer connection's, which
//! tags its messages with soju.im/network. Messages from the client with
//! that tag are handled as messages of the virtual connection.

use super::*;

pub const MULTIPLEX_CAP: &str = "soju.im/multiplex";
pub const NETWORK_TAG: &str = "soju.im/network";

/// Capabilities of the bouncer connection which a virtual connection
/// doesn't inherit: like a connection bound to a network, it doesn't get
/// network notifications, and can't attach networks.
const NOT_INHERITED_CAPS: &[&str] = &[MULTIPLEX_CAP, "soju.im/bouncer-networks-notify"];

/// The virtual connection of a network attached to a bouncer connection.
pub fn find_attached(u: &User, parent_id: u64, net_id: i64) -> Option<u64> {
    u.downstreams
        .iter()
        .find(|d| {
            d.parent_id == Some(parent_id) && d.network_id == Some(net_id) && !d.conn.is_closed()
        })
        .map(|d| d.id)
}

/// Routes a message from a downstream connection: a message tagged with a
/// network goes to the virtual connection of that network. Returns the
/// connection which handles the message, or None if it was answered with
/// an error.
pub fn route_message(u: &mut User, dc_id: u64, mut msg: Message) -> Option<(u64, Message)> {
    let Some(i) = u.dc_index(dc_id) else {
        return Some((dc_id, msg));
    };
    let Some(tag) = msg.tags.get(NETWORK_TAG).map(str::to_owned) else {
        return Some((dc_id, msg));
    };
    if !u.downstreams[i].caps.is_enabled(MULTIPLEX_CAP) {
        return Some((dc_id, msg));
    }
    msg.tags.remove(NETWORK_TAG);
    let child = tag
        .parse::<i64>()
        .ok()
        .and_then(|net_id| find_attached(u, dc_id, net_id));
    if let Some(child) = child {
        return Some((child, msg));
    }
    // Late messages for a network the client just detached are expected.
    // The reply (an ACK for PING/PONG) carries the message's label.
    let dc = &mut u.downstreams[i];
    begin_message(dc, &msg);
    let res = if msg.command == "PING" || msg.command == "PONG" {
        Ok(())
    } else {
        Err(cmds::fail(&[
            &msg.command,
            "NETWORK_NOT_ATTACHED",
            &tag,
            "Network not attached",
        ]))
    };
    // Only IRC errors: nothing to propagate
    let _ = finish_message(dc, res);
    None
}

/// Attaches a network to a bouncer connection: creates its virtual
/// connection, which gets the welcome burst of a connection bound to it.
pub fn attach_network(u: &mut User, parent_id: u64, net_id: i64) {
    if let Some(old) = find_attached(u, parent_id, net_id) {
        detach_network(u, old);
    }
    let i = u.dc_index(parent_id).unwrap();
    let id = u.srv.next_downstream_id();
    let tx = u.tx.clone();
    let parent = &u.downstreams[i];
    let conn = parent.conn.view(NETWORK_TAG, &net_id.to_string(), move || {
        let _ = tx.send(crate::user::Event::DownstreamDisconnected { dc_id: id });
    });
    let mut dc = DownstreamConn::new(
        parent.srv.clone(),
        conn,
        parent.info.clone(),
        id,
        parent.logger.clone(),
    );
    dc.parent_id = Some(parent_id);
    dc.registered = true;
    dc.client_name = parent.client_name.clone();
    dc.impersonating = parent.impersonating;
    dc.nick = parent.nick.clone();
    dc.nick_cm = parent.nick_cm.clone();
    dc.realname = parent.realname.clone();
    dc.username = parent.username.clone();
    dc.away = parent.away.clone();
    dc.cap_version = parent.cap_version;
    dc.caps = parent.caps.clone();
    for cap in NOT_INHERITED_CAPS {
        dc.caps.enabled.shift_remove(*cap);
    }
    dc.registration = Some(Registration {
        nick: parent.nick.clone(),
        username: parent.username.clone(),
        network_id: net_id,
        auth_username: parent.username.clone(),
        ..Default::default()
    });
    u.downstream_connected(dc);
}

/// Removes the virtual connection of an attached network, without telling
/// the client: it asked for it.
pub fn detach_network(u: &mut User, dc_id: u64) {
    if let Some(i) = u.dc_index(dc_id) {
        u.downstreams[i].conn.close_quietly();
    }
    u.remove_downstream(dc_id);
}
