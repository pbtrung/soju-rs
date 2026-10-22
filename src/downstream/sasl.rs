//! SASL authentication of downstream connections.

use super::*;

#[derive(Clone, Debug)]
pub struct SaslPlain {
    pub identity: String,
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Default)]
pub struct OAuthBearerOptions {
    pub username: String,
    pub token: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaslServerKind {
    Plain,
    OAuthBearer,
    Anonymous,
    External,
}

#[derive(Clone, Debug)]
pub struct DownstreamSasl {
    kind: SaslServerKind,
    pub mechanism: String,
    done: bool,
    oauth_fail: Option<String>,
    pub plain: Option<SaslPlain>,
    pub oauth_bearer: Option<OAuthBearerOptions>,
    pending_resp: String,
}

impl DownstreamSasl {
    /// Feeds a client response to the SASL server. Returns the challenge to
    /// send back, or None when the exchange is complete.
    fn next(&mut self, response: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
        if self.kind == SaslServerKind::OAuthBearer
            && let Some(fail) = &self.oauth_fail
        {
            if response.is_none_or(|r| r.len() != 1 && r.first() != Some(&0x01)) {
                bail!("sasl: invalid response");
            }
            bail!("{fail}");
        }
        if self.done {
            bail!("sasl: unexpected client response");
        }
        let Some(response) = response else {
            return Ok(Some(Vec::new()));
        };
        self.done = true;
        match self.kind {
            SaslServerKind::Plain => {
                let parts: Vec<&[u8]> = response.split(|&b| b == 0).collect();
                if parts.len() != 3 {
                    bail!("sasl: invalid response");
                }
                let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
                self.plain = Some(SaslPlain {
                    identity: s(parts[0]),
                    username: s(parts[1]),
                    password: s(parts[2]),
                });
                Ok(None)
            }
            SaslServerKind::Anonymous => Ok(None),
            SaslServerKind::External => {
                if response.contains(&0) {
                    bail!("sasl: identity contains a NUL character");
                }
                Ok(None)
            }
            SaslServerKind::OAuthBearer => {
                let fail = |this: &mut DownstreamSasl, descr: &str| -> Result<Option<Vec<u8>>> {
                    this.oauth_fail = Some(format!("sasl: client error: {descr}"));
                    Ok(Some(
                        br#"{"status":"invalid_request","schemes":"bearer","scope":""}"#.to_vec(),
                    ))
                };
                let mut parts = response.splitn(3, |&b| b == b',');
                let (Some(flag), Some(authzid), Some(rest)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return fail(self, "Invalid response");
                };
                if flag != b"n" {
                    return fail(self, "Invalid response, missing 'n' in gs2-cb-flag");
                }
                let mut opts = OAuthBearerOptions::default();
                if !authzid.is_empty() {
                    let Some(user) = authzid.strip_prefix(b"a=") else {
                        return fail(self, "Invalid response, missing 'a=' in gs2-authzid");
                    };
                    opts.username = String::from_utf8_lossy(user).into_owned();
                }
                for p in rest.split(|&b| b == 0x01) {
                    if p.is_empty() {
                        continue;
                    }
                    let p = String::from_utf8_lossy(p);
                    let Some((k, v)) = p.split_once('=') else {
                        return fail(self, "Invalid response, missing '='");
                    };
                    match k {
                        "host" => {}
                        "port" => {
                            if v.parse::<u16>().is_err() {
                                return fail(self, "Invalid response, malformed 'port' value");
                            }
                        }
                        "auth" => {
                            const PREFIX: &str = "bearer ";
                            if !v.to_lowercase().starts_with(PREFIX) {
                                return fail(self, "Unsupported token type");
                            }
                            opts.token = v[PREFIX.len()..].to_owned();
                        }
                        _ => {
                            return fail(
                                self,
                                &format!("Invalid response, unknown parameter: {k}"),
                            );
                        }
                    }
                }
                self.oauth_bearer = Some(opts);
                Ok(None)
            }
        }
    }
}

impl DownstreamConn {
    /// Handles an AUTHENTICATE message. Returns the completed SASL state
    /// once the client has sent its credentials.
    pub fn handle_authenticate(&mut self, msg: &Message) -> Result<Option<DownstreamSasl>> {
        let res = self.handle_authenticate_inner(msg);
        if res.is_err() {
            self.sasl = None;
        }
        res
    }

    fn handle_authenticate_inner(&mut self, msg: &Message) -> Result<Option<DownstreamSasl>> {
        let nick = self.nick.clone();
        if !self.caps.is_enabled("sasl") {
            return Err(irc_error(msg!(
                ERR_SASLFAIL,
                nick,
                "AUTHENTICATE requires the \"sasl\" capability to be enabled"
            )));
        }
        let Some(param) = msg.params.first() else {
            return Err(irc_error(msg!(
                ERR_SASLFAIL,
                nick,
                "Missing AUTHENTICATE argument"
            )));
        };
        if param == "*" {
            return Err(irc_error(msg!(
                ERR_SASLABORTED,
                nick,
                "SASL authentication aborted"
            )));
        }

        let mut resp: Option<Vec<u8>> = None;
        match &mut self.sasl {
            None => {
                let mech = param.to_uppercase();
                let kind = match mech.as_str() {
                    "PLAIN" => SaslServerKind::Plain,
                    "OAUTHBEARER" => SaslServerKind::OAuthBearer,
                    "ANONYMOUS" => SaslServerKind::Anonymous,
                    "EXTERNAL" => SaslServerKind::External,
                    _ => {
                        return Err(irc_error(msg!(
                            ERR_SASLFAIL,
                            nick,
                            "Unsupported SASL mechanism"
                        )));
                    }
                };
                self.sasl = Some(DownstreamSasl {
                    kind,
                    mechanism: mech,
                    done: false,
                    oauth_fail: None,
                    plain: None,
                    oauth_bearer: None,
                    pending_resp: String::new(),
                });
            }
            Some(sasl) => {
                let chunk = if param == "+" { "" } else { param.as_str() };
                if sasl.pending_resp.len() + chunk.len() > 10 * 1024 {
                    return Err(irc_error(msg!(ERR_SASLFAIL, nick, "Response too long")));
                }
                sasl.pending_resp.push_str(chunk);
                if chunk.len() == xirc::MAX_SASL_LENGTH {
                    return Ok(None); // Multi-line response, wait for the next command
                }
                let decoded = BASE64.decode(&sasl.pending_resp).map_err(|_| {
                    irc_error(msg!(
                        ERR_SASLFAIL,
                        nick.clone(),
                        "Invalid base64-encoded response"
                    ))
                })?;
                sasl.pending_resp.clear();
                resp = Some(decoded);
            }
        }

        let sasl = self.sasl.as_mut().unwrap();
        match sasl.next(resp.as_deref())? {
            None => Ok(Some(sasl.clone())),
            Some(challenge) => {
                let s = if challenge.is_empty() {
                    "+".to_owned()
                } else {
                    BASE64.encode(&challenge)
                };
                // TODO: multi-line messages
                self.send_message(msg!("AUTHENTICATE", s));
                Ok(None)
            }
        }
    }

    pub(super) fn set_auth_username(&mut self, username: &str, client: &str, network: &str) {
        self.client_name = client.to_owned();
        let r = self.registration.as_mut().unwrap();
        r.auth_username = username.to_owned();
        r.network_name = network.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sasl_mechanisms() {
        let mut s = DownstreamSasl {
            kind: SaslServerKind::Plain,
            mechanism: "PLAIN".into(),
            done: false,
            oauth_fail: None,
            plain: None,
            oauth_bearer: None,
            pending_resp: String::new(),
        };
        assert_eq!(s.next(None).unwrap(), Some(vec![]));
        assert_eq!(s.next(Some(b"\0alice\0pw")).unwrap(), None);
        assert_eq!(s.plain.as_ref().unwrap().username, "alice");
        assert!(s.next(Some(b"x")).is_err());

        let mut s = DownstreamSasl {
            kind: SaslServerKind::OAuthBearer,
            mechanism: "OAUTHBEARER".into(),
            done: false,
            oauth_fail: None,
            plain: None,
            oauth_bearer: None,
            pending_resp: String::new(),
        };
        assert_eq!(
            s.next(Some(b"n,a=alice,\x01auth=Bearer tok\x01\x01"))
                .unwrap(),
            None
        );
        let o = s.oauth_bearer.unwrap();
        assert_eq!((o.username.as_str(), o.token.as_str()), ("alice", "tok"));
    }
}
