use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use super::{
    ChannelStatus, Isupport, MAX_MESSAGE_LENGTH, MAX_MESSAGE_PARAMS, MAX_SASL_LENGTH,
    encode_isupport_value,
};
use crate::irc::{Message, numerics::*};
use crate::msg;

/// Generates JOIN messages for a list of channels, splitting them so that
/// each message fits in the maximum message length. Channels with a key
/// are put first.
pub fn generate_join(channels: &[String], keys: &[String]) -> Vec<Message> {
    let mut pairs: Vec<(&String, &String)> = channels.iter().zip(keys.iter()).collect();
    pairs.sort_by(|(ca, ka), (cb, kb)| {
        let (ha, hb) = (!ka.is_empty(), !kb.is_empty());
        if ha != hb {
            // Only one of the channels has a key
            return hb.cmp(&ha);
        }
        ca.cmp(cb)
    });

    // Two spaces because there are three words (JOIN, channels and keys)
    let max_length = MAX_MESSAGE_LENGTH - ("JOIN".len() + 2);

    let mut msgs = Vec::new();
    let mut channels_buf = String::new();
    let mut keys_buf = String::new();
    let flush = |channels_buf: &mut String, keys_buf: &mut String, msgs: &mut Vec<Message>| {
        let mut params = vec![std::mem::take(channels_buf)];
        if !keys_buf.is_empty() {
            params.push(std::mem::take(keys_buf));
        }
        msgs.push(Message::new("JOIN", params));
    };
    for (channel, key) in pairs {
        let mut n = channels_buf.len() + keys_buf.len() + 1 + channel.len();
        if !key.is_empty() {
            n += 1 + key.len();
        }

        if !channels_buf.is_empty() && n > max_length {
            // No room for the new channel in this message
            flush(&mut channels_buf, &mut keys_buf, &mut msgs);
        }

        if !channels_buf.is_empty() {
            channels_buf.push(',');
        }
        channels_buf.push_str(channel);
        if !key.is_empty() {
            if !keys_buf.is_empty() {
                keys_buf.push(',');
            }
            keys_buf.push_str(key);
        }
    }
    if !channels_buf.is_empty() {
        flush(&mut channels_buf, &mut keys_buf, &mut msgs);
    }
    msgs
}

/// Generates RPL_ISUPPORT messages. Tokens are sorted.
pub fn generate_isupport(isupport: &Isupport) -> Vec<Message> {
    let mut tokens: Vec<String> = isupport
        .iter()
        .map(|(k, v)| match v {
            Some(v) => format!("{k}={v}"),
            None => k.clone(),
        })
        .collect();
    tokens.sort();

    let max_tokens = MAX_MESSAGE_PARAMS - 2; // 2 reserved params: nick + text

    // TODO: take into account MAX_MESSAGE_LENGTH as well
    tokens
        .chunks(max_tokens)
        .map(|chunk| {
            let mut params = vec!["*".to_owned()];
            params.extend(chunk.iter().map(|tok| encode_isupport_value(tok)));
            params.push("are supported".to_owned());
            Message::new(RPL_ISUPPORT, params)
        })
        .collect()
}

pub fn generate_motd(motd: &str) -> Vec<Message> {
    let mut msgs = vec![msg!(RPL_MOTDSTART, "*", "- Message of the Day -")];
    for l in motd.split('\n') {
        msgs.push(msg!(RPL_MOTD, "*", l));
    }
    msgs.push(msg!(RPL_ENDOFMOTD, "*", "End of /MOTD command."));
    msgs
}

pub fn generate_monitor(subcmd: &str, targets: &[String]) -> Vec<Message> {
    let max_length = MAX_MESSAGE_LENGTH - format!("MONITOR {subcmd} ").len();

    let mut msgs = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut n = 0;
    for target in targets {
        if !buf.is_empty() && n + target.len() + 1 > max_length {
            msgs.push(msg!("MONITOR", subcmd, buf.join(",")));
            buf.clear();
            n = 0;
        }
        buf.push(target);
        n += target.len() + 1;
    }
    if !buf.is_empty() {
        msgs.push(msg!("MONITOR", subcmd, buf.join(",")));
    }
    msgs
}

pub fn generate_names_reply(
    channel: &str,
    status: ChannelStatus,
    members: &[String],
) -> Vec<Message> {
    let empty = msg!(RPL_NAMREPLY, "*", status.as_str(), channel, "");
    let max_length = MAX_MESSAGE_LENGTH.saturating_sub(empty.to_string().len());

    let mut msgs = Vec::new();
    let mut buf = String::new();
    for s in members {
        let n = buf.len() + 1 + s.len();
        if !buf.is_empty() && n > max_length {
            // There's not enough space for the next space + nick
            msgs.push(msg!(
                RPL_NAMREPLY,
                "*",
                status.as_str(),
                channel,
                std::mem::take(&mut buf)
            ));
        }
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(s);
    }
    if !buf.is_empty() {
        msgs.push(msg!(RPL_NAMREPLY, "*", status.as_str(), channel, buf));
    }
    msgs.push(msg!(RPL_ENDOFNAMES, "*", channel, "End of /NAMES list"));
    msgs
}

/// Generates AUTHENTICATE messages for a SASL response, split in chunks.
pub fn generate_sasl(resp: &[u8]) -> Vec<Message> {
    let encoded = BASE64.encode(resp);

    // <= instead of < because we need to send a final empty response if
    // the last chunk is exactly 400 bytes long
    let mut msgs = Vec::new();
    let mut i = 0;
    while i <= encoded.len() {
        let j = (i + MAX_SASL_LENGTH).min(encoded.len());
        let chunk = &encoded[i..j];
        let chunk = if chunk.is_empty() { "+" } else { chunk };
        msgs.push(msg!("AUTHENTICATE", chunk));
        i += MAX_SASL_LENGTH;
    }
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_sorting_and_split() {
        let channels = vec!["#b".to_owned(), "#a".to_owned(), "#c".to_owned()];
        let keys = vec!["".to_owned(), "".to_owned(), "key".to_owned()];
        let msgs = generate_join(&channels, &keys);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].to_string(), "JOIN #c,#a,#b key");

        let channels: Vec<String> = (0..100).map(|i| format!("#channel{i:03}")).collect();
        let keys = vec![String::new(); 100];
        let msgs = generate_join(&channels, &keys);
        assert!(msgs.len() > 1);
        for m in &msgs {
            assert!(m.to_string().len() <= MAX_MESSAGE_LENGTH);
        }
        let total: usize = msgs.iter().map(|m| m.params[0].split(',').count()).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn sasl_chunks() {
        assert_eq!(generate_sasl(b"").len(), 1);
        assert_eq!(generate_sasl(b"")[0].params[0], "+");
        // 300 bytes -> 400 base64 chars -> one full chunk plus "+"
        let msgs = generate_sasl(&[0u8; 300]);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].params[0], "+");
        let msgs = generate_sasl(&[0u8; 301]);
        assert_eq!(msgs.len(), 2);
        assert_ne!(msgs[1].params[0], "+");
    }

    #[test]
    fn isupport() {
        let mut is = Isupport::new();
        for i in 0..20 {
            is.insert(format!("T{i:02}"), None);
        }
        is.insert("NETWORK".into(), Some("a b".into()));
        let msgs = generate_isupport(&is);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].params.len(), 15);
        assert_eq!(msgs[0].params[1], "NETWORK=a\\x20b");
    }

    #[test]
    fn names() {
        let members: Vec<String> = (0..200).map(|i| format!("@nick{i}")).collect();
        let msgs = generate_names_reply("#chan", ChannelStatus::Public, &members);
        assert_eq!(msgs.last().unwrap().command, RPL_ENDOFNAMES);
        for m in &msgs {
            assert!(m.to_string().len() <= MAX_MESSAGE_LENGTH);
        }
    }

    #[test]
    fn monitor() {
        let targets: Vec<String> = (0..200).map(|i| format!("nick{i}")).collect();
        let msgs = generate_monitor("+", &targets);
        assert!(msgs.len() > 1);
        for m in &msgs {
            assert!(m.to_string().len() <= MAX_MESSAGE_LENGTH);
        }
    }
}
