use crate::irc::{Message, numerics::*};

/// All WHOX field letters, by order of appearance in RPL_WHOSPCRPL messages.
const WHOX_FIELDS: &[u8] = b"tcuihsnfdlaor";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WhoxInfo {
    pub token: String,
    pub channel: String,
    pub username: String,
    pub hostname: String,
    pub server: String,
    pub nickname: String,
    pub flags: String,
    pub account: String,
    pub realname: String,
}

fn escape_hostname(hostname: &str) -> String {
    // The hostname cannot start with a colon as this would get parsed as a
    // trailing parameter. IPv6 addresses such as "::1" are prefixed with a
    // zero to ensure this.
    if hostname.starts_with(':') {
        format!("0{hostname}")
    } else {
        hostname.to_owned()
    }
}

impl WhoxInfo {
    fn get(&self, k: u8) -> String {
        match k {
            b't' => self.token.clone(),
            b'c' => {
                if self.channel.is_empty() {
                    "*".to_owned()
                } else {
                    self.channel.clone()
                }
            }
            b'u' => self.username.clone(),
            b'i' => "255.255.255.255".to_owned(),
            b'h' => escape_hostname(&self.hostname),
            b's' => self.server.clone(),
            b'n' => self.nickname.clone(),
            b'f' => self.flags.clone(),
            b'd' | b'l' | b'o' => "0".to_owned(),
            b'a' => {
                // WHOX uses "0" to mean "no account"
                if !self.account.is_empty() && self.account != "*" {
                    self.account.clone()
                } else {
                    "0".to_owned()
                }
            }
            b'r' => self.realname.clone(),
            _ => String::new(),
        }
    }

    fn set(&mut self, k: u8, v: String) {
        match k {
            b't' => self.token = v,
            b'c' => self.channel = v,
            b'u' => self.username = v,
            b'h' => self.hostname = v,
            b's' => self.server = v,
            b'n' => self.nickname = v,
            b'f' => self.flags = v,
            b'a' => self.account = v,
            b'r' => self.realname = v,
            _ => {}
        }
    }
}

pub fn generate_whox_reply(fields: &str, info: &WhoxInfo) -> Message {
    if fields.is_empty() {
        let channel = if info.channel.is_empty() {
            "*"
        } else {
            &info.channel
        };
        return crate::msg!(
            RPL_WHOREPLY,
            "*",
            channel,
            info.username.as_str(),
            escape_hostname(&info.hostname),
            info.server.as_str(),
            info.nickname.as_str(),
            info.flags.as_str(),
            format!("0 {}", info.realname)
        );
    }

    let field_set = fields.as_bytes();
    let mut params = vec!["*".to_owned()];
    for &field in WHOX_FIELDS {
        if field_set.contains(&field) {
            params.push(info.get(field));
        }
    }
    Message::new(RPL_WHOSPCRPL, params)
}

/// Parses WHO options, returning the WHOX fields and token.
pub fn parse_whox_options(options: &str) -> (String, String) {
    let mut fields = String::new();
    let mut token = String::new();
    // TODO: add support for WHOX flags before the '%'
    if let Some((_, rest)) = options.split_once('%') {
        let (f, t) = match rest.split_once(',') {
            Some((f, t)) => (f, Some(t)),
            None => (rest, None),
        };
        fields = f.to_lowercase();
        if let Some(t) = t
            && fields.contains('t')
        {
            token = t.to_owned();
        }
    }
    (fields, token)
}

pub fn parse_whox_reply(msg: &Message, fields: &str) -> Result<WhoxInfo, String> {
    if msg.command != RPL_WHOSPCRPL {
        return Err(format!("invalid WHOX reply {:?}", msg.command));
    }
    if msg.params.is_empty() {
        return Err("invalid RPL_WHOSPCRPL: no params".to_owned());
    }

    let field_set = fields.as_bytes();
    let mut info = WhoxInfo::default();
    let mut values = msg.params[1..].iter();
    for &field in WHOX_FIELDS {
        if !field_set.contains(&field) {
            continue;
        }
        let Some(v) = values.next() else {
            return Err(format!(
                "invalid RPL_WHOSPCRPL: missing value for field {:?}",
                field as char
            ));
        };
        info.set(field, v.clone());
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        assert_eq!(parse_whox_options(""), (String::new(), String::new()));
        assert_eq!(
            parse_whox_options("%tnA,42"),
            ("tna".to_owned(), "42".to_owned())
        );
        assert_eq!(
            parse_whox_options("%na,42"),
            ("na".to_owned(), String::new())
        );
    }

    #[test]
    fn roundtrip() {
        let info = WhoxInfo {
            token: "42".into(),
            channel: "#a".into(),
            username: "u".into(),
            hostname: "::1".into(),
            server: "s".into(),
            nickname: "n".into(),
            flags: "H".into(),
            account: "acc".into(),
            realname: "real name".into(),
        };
        let m = generate_whox_reply("tcuhsnfar", &info);
        assert_eq!(m.to_string(), "354 * 42 #a u 0::1 s n H acc :real name");
        let parsed = parse_whox_reply(&m, "tcuhsnfar").unwrap();
        assert_eq!(parsed.nickname, "n");
        assert_eq!(parsed.hostname, "0::1");
        assert_eq!(parsed.realname, "real name");

        let m = generate_whox_reply("", &info);
        assert_eq!(m.to_string(), "352 * #a u 0::1 s n H :0 real name");
    }
}
