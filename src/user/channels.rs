//! Saved channels: storing, detaching and attaching them.

use anyhow::{Result, bail};

use super::{User, dcs_for};
use crate::downstream;
use crate::upstream;
use crate::xirc::CaseMapping;
use crate::{logf, msg};

impl User {
    pub fn store_channel(&mut self, ni: usize, name: &str) {
        let net = &mut self.networks[ni];
        let net_id = net.id();
        if let Some(ch) = net.channels.get_mut(name)
            && let Err(e) = self.srv.db.store_channel(net_id, ch)
        {
            logf!(
                self.logger,
                "failed to store updated channel {:?}: {e}",
                ch.name
            );
        }
    }

    /// Detaches a channel.
    pub fn detach_channel(&mut self, ni: usize, name: &str) {
        let net = &mut self.networks[ni];
        let Some(ch) = net.channels.get_mut(name) else {
            return;
        };
        if ch.detached {
            return;
        }
        logf!(net.logger, "detaching channel {:?}", ch.name);
        ch.detached = true;
        let ch_name = ch.name.clone();

        let name_cm = (net.casemap)(&ch_name);
        let id = match self.msg_store.last_msg_id(&net.record, &name_cm) {
            Ok(id) => id,
            Err(e) => {
                logf!(
                    net.logger,
                    "failed to get last message ID for channel {ch_name:?}: {e}"
                );
                String::new()
            }
        };
        net.channels.get_mut(name).unwrap().detached_internal_msg_id = id;

        if let Some(uc) = net.upstream_mut()
            && let Some(uch) = uc.channels.get_mut(&ch_name)
        {
            uch.stop_auto_detach();
        }

        let net_id = net.id();
        for dc in dcs_for(&mut self.downstreams, net_id) {
            let prefix = dc.prefix();
            dc.send_message(msg!("PART", ch_name.clone(), "Detach").with_prefix(prefix));
        }
    }

    /// Attaches a detached channel.
    pub fn attach_channel(&mut self, ni: usize, name: &str) {
        let net = &mut self.networks[ni];
        let Some(ch) = net.channels.get_mut(name) else {
            return;
        };
        if !ch.detached {
            return;
        }
        logf!(net.logger, "attaching channel {:?}", ch.name);
        let detached_msg_id = std::mem::take(&mut ch.detached_internal_msg_id);
        ch.detached = false;
        let ch_name = ch.name.clone();

        let net_id = net.id();
        upstream::update_channel_auto_detach(self, ni, &ch_name);

        let dc_ids: Vec<u64> = dcs_for(&mut self.downstreams, net_id)
            .map(|d| d.id)
            .collect();
        for dc_id in dc_ids {
            let Some(i) = self.dc_index(dc_id) else {
                continue;
            };
            let dc = &mut self.downstreams[i];
            let prefix = dc.prefix();
            dc.send_message(msg!("JOIN", ch_name.clone()).with_prefix(prefix));

            let net = &self.networks[ni];
            if let Some(uc) = net.upstream()
                && let Some(uch) = uc.channels.get(&ch_name)
            {
                downstream::forward_channel(
                    &self.srv.db,
                    &mut self.downstreams[i],
                    uch,
                    net.id(),
                    net.casemap,
                );
            }

            if !detached_msg_id.is_empty() {
                downstream::send_target_backlog(self, i, ni, &ch_name, &detached_msg_id);
            }
        }
    }

    pub fn delete_channel(&mut self, ni: usize, name: &str) -> Result<()> {
        let net = &mut self.networks[ni];
        let Some(ch) = net.channels.get(name) else {
            bail!("unknown channel {name:?}");
        };
        let ch_id = ch.id;
        let ch_name = ch.name.clone();
        if let Some(uc) = net.upstream_mut()
            && let Some(uch) = uc.channels.get_mut(&ch_name)
        {
            uch.stop_auto_detach();
        }
        self.srv.db.delete_channel(ch_id)?;
        self.networks[ni].channels.del(name);
        Ok(())
    }

    pub fn update_casemapping(&mut self, ni: usize, cm: CaseMapping) {
        let net = &mut self.networks[ni];
        net.casemap = cm;
        net.channels.set_casemapping(cm);
        net.delivered.m.set_casemapping(cm);
        net.push_targets.lock().unwrap().set_casemapping(cm);
        if let Some(uc) = &mut net.conn {
            uc.channels.set_casemapping(cm);
            for uch in uc.channels.values_mut() {
                uch.members.set_casemapping(cm);
            }
            uc.users.set_casemapping(cm);
            uc.monitored.set_casemapping(cm);
        }
        let net_id = net.id();
        for dc in dcs_for(&mut self.downstreams, net_id) {
            dc.update_casemapping(cm);
        }
    }
}
