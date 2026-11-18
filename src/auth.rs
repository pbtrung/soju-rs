//! Authentication backends.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::config::{self, AuthDriver};
use crate::database::{Database, split_host_port};

mod oauth;
pub use oauth::OAuth2;

/// An authentication error. The internal error is logged, the external
/// message can safely be sent to the user.
#[derive(Debug)]
pub struct AuthError {
    pub internal: anyhow::Error,
    pub external_msg: String,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.internal)
    }
}

impl std::error::Error for AuthError {}

/// Wraps an error and indicates to the user that the provided credentials
/// were invalid.
#[must_use]
pub fn invalid_credentials(err: anyhow::Error) -> anyhow::Error {
    AuthError {
        internal: err,
        external_msg: "Invalid credentials".to_owned(),
    }
    .into()
}

/// Returns the user-friendly reason of an authentication failure.
#[must_use]
pub fn auth_error_reason(err: &anyhow::Error) -> String {
    match err.downcast_ref::<AuthError>() {
        Some(e) => e.external_msg.clone(),
        None => "Authentication failed".to_owned(),
    }
}

///
/// # Errors
///
/// Returns an error if configuration, credentials, discovery, or the authentication backend cannot be validated.
pub fn http_client() -> Result<reqwest::Client> {
    http_client_with_builder(reqwest::Client::builder())
}

fn http_client_with_builder(builder: reqwest::ClientBuilder) -> Result<reqwest::Client> {
    Ok(builder
        .use_preconfigured_tls(rustls_client_config()?)
        .timeout(Duration::from_secs(30))
        .build()?)
}

/// A TLS client configuration for HTTP/1.1 clients.
pub(crate) fn rustls_client_config() -> Result<rustls::ClientConfig> {
    let cfg = crate::tls::client_config("", None)?;
    let mut cfg = (*cfg).clone();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

fn set_forwarded(req: reqwest::RequestBuilder, addr: &str) -> reqwest::RequestBuilder {
    if addr.is_empty() {
        return req;
    }
    let host = split_host_port(addr).map_or_default(|(h, _)| h);
    req.header("Forwarded", format!("for={addr:?}"))
        .header("X-Forwarded-For", host)
}

#[derive(Clone)]
pub enum PlainAuthenticator {
    Internal,
    Http {
        url: String,
        client: reqwest::Client,
    },
    /// PLAIN authentication with an OAuth 2.0 token as password
    OAuthPlain(Arc<OAuth2>),
}

#[derive(Clone, Default)]
pub struct Authenticator {
    pub plain: Option<PlainAuthenticator>,
    pub oauth_bearer: Option<Arc<OAuth2>>,
}

impl Authenticator {
    #[must_use]
    pub fn internal() -> Authenticator {
        Authenticator {
            plain: Some(PlainAuthenticator::Internal),
            oauth_bearer: None,
        }
    }

    /// Builds the authenticator from config directives.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration, credentials, discovery, or the authentication backend cannot be validated.
    pub async fn from_config(auths: &[config::Auth]) -> Result<Authenticator> {
        let mut out = Authenticator::default();
        for a in auths {
            let (plain, bearer) = match a.driver {
                AuthDriver::Internal => (Some(PlainAuthenticator::Internal), None),
                AuthDriver::Http => (
                    Some(PlainAuthenticator::Http {
                        url: a.source.clone(),
                        client: http_client()?,
                    }),
                    None,
                ),
                AuthDriver::OAuth2 => (None, Some(Arc::new(OAuth2::new(&a.source).await?))),
                AuthDriver::Pam => {
                    bail!("failed to create authenticator pam: PAM support is disabled")
                }
            };
            if let Some(p) = plain {
                if out.plain.is_some() {
                    bail!(
                        "failed to load authenticators: multiple plain authentication methods \
                            specified"
                    );
                }
                out.plain = Some(p);
            }
            if let Some(b) = bearer {
                if out.oauth_bearer.is_some() {
                    bail!(
                        "failed to load authenticators: multiple OAuth authentication methods \
                            specified"
                    );
                }
                out.oauth_bearer = Some(b);
            }
        }
        if let (Some(b), None) = (&out.oauth_bearer, &out.plain) {
            out.plain = Some(PlainAuthenticator::OAuthPlain(b.clone()));
        }
        Ok(out)
    }
}

impl PlainAuthenticator {
    /// Checks credentials. `remote_addr` is the downstream address, passed to
    /// external services in Forwarded headers.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration, credentials, discovery, or the authentication backend cannot be validated.
    pub async fn auth_plain(
        &self,
        db: &Arc<Database>,
        username: &str,
        password: &str,
        remote_addr: &str,
    ) -> Result<()> {
        match self {
            PlainAuthenticator::Internal => {
                // Only hashing holds a CPU permit, not the database queries
                let name = username.to_owned();
                let Some(mut u) = crate::database::run(db, move |db| db.get_user(&name)).await?
                else {
                    return Err(invalid_credentials(anyhow!(
                        "user not found: sql: no rows in result set"
                    )));
                };
                let old = u.password.clone();
                let password = password.to_owned();
                let (u, upgraded) = crate::util::cpu_work(move || {
                    let upgraded = u.check_password(&password);
                    (u, upgraded)
                })
                .await?;
                if upgraded.map_err(invalid_credentials)? {
                    // Only the hash: the rest of the record may be updated
                    // meanwhile by the user's event loop
                    let (id, new) = (u.id, u.password);
                    crate::database::run(db, move |db| db.upgrade_user_password(id, &old, &new))
                        .await?;
                }
                Ok(())
            }
            PlainAuthenticator::Http { url, client } => {
                let req = client.post(url).header(
                    "Authorization",
                    format!("Basic {}", BASE64.encode(format!("{username}:{password}"))),
                );
                let resp = set_forwarded(req, remote_addr)
                    .send()
                    .await
                    .map_err(|e| anyhow!("failed to send HTTP auth request: {e}"))?;
                match resp.status().as_u16() {
                    200 => Ok(()),
                    403 => Err(invalid_credentials(anyhow!(
                        "HTTP auth server returned forbidden"
                    ))),
                    _ => bail!("HTTP auth error: {}", resp.status()),
                }
            }
            PlainAuthenticator::OAuthPlain(oauth) => {
                let effective = oauth.auth_oauth_bearer(password, remote_addr).await?;
                if username != effective {
                    return Err(invalid_credentials(anyhow!(
                        "username mismatch from OAuth 2.0 provider"
                    )));
                }
                Ok(())
            }
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Exactly two hex digits: from_str_radix would also accept a sign,
        // e.g. "%+f"
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "The surrounding length check or bounded iteration establishes this local collection index."
            )]
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Ok(v) = hex::decode(&b[i + 1..i + 3])
            {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The surrounding length check or bounded iteration establishes this local collection index."
                    )]
                    out.push(v[0]);
                }
                i += 3;
                continue;
            }
        }
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "The surrounding length check or bounded iteration establishes this local collection index."
            )]
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escapes a string like Go's url.QueryEscape.
#[must_use]
pub fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                #[expect(
                    clippy::expect_used,
                    reason = "Formatting a byte into String cannot fail."
                )]
                write!(out, "%{b:02X}").expect("formatting a byte into String");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::User;

    #[tokio::test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    async fn internal() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let mut u = User::new("alice");
        u.set_password("secret").unwrap();
        db.store_user(&mut u).unwrap();
        let a = PlainAuthenticator::Internal;
        a.auth_plain(&db, "alice", "secret", "").await.unwrap();
        let err = a.auth_plain(&db, "alice", "nope", "").await.unwrap_err();
        assert_eq!(auth_error_reason(&err), "Invalid credentials");
        let err = a.auth_plain(&db, "bob", "secret", "").await.unwrap_err();
        assert_eq!(auth_error_reason(&err), "Invalid credentials");
        assert_eq!(auth_error_reason(&anyhow!("x")), "Authentication failed");
    }

    #[tokio::test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    async fn password_upgrade_kept() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let mut u = User::new("alice");
        u.password = bcrypt::hash("secret", 4).unwrap();
        db.store_user(&mut u).unwrap();
        let a = PlainAuthenticator::Internal;
        a.auth_plain(&db, "alice", "secret", "").await.unwrap();
        let upgraded = db.get_user("alice").unwrap().unwrap().password;
        assert_ne!(upgraded, u.password);
        // A live user's record still holds the old hash
        u.realname = "Alice".to_owned();
        db.store_user_keep_password(&u).unwrap();
        db.set_user_downstream_interacted_at(u.id, jiff::Timestamp::now())
            .unwrap();
        let stored = db.get_user("alice").unwrap().unwrap();
        assert_eq!(stored.password, upgraded);
        assert_eq!(stored.realname, "Alice");
        a.auth_plain(&db, "alice", "secret", "").await.unwrap();
    }

    #[test]
    fn escape() {
        assert_eq!(query_escape("a b/c+d"), "a+b%2Fc%2Bd");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%3a%3A"), "::");
        assert_eq!(percent_decode("a%+fb%2"), "a%+fb%2");
    }

    #[tokio::test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    #[expect(
        clippy::indexing_slicing,
        reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
    )]
    async fn http_auth() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = ln.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let ok = req.contains(&format!("Basic {}", BASE64.encode("alice:pw")));
                assert!(
                    req.contains("X-Forwarded-For: 1.2.3.4")
                        || req.contains("x-forwarded-for: 1.2.3.4")
                );
                let status = if ok { "200 OK" } else { "403 Forbidden" };
                s.write_all(
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            }
        });
        let db = Arc::new(Database::open_in_memory().unwrap());
        let a = PlainAuthenticator::Http {
            url: format!("http://{addr}/auth"),
            client: http_client().unwrap(),
        };
        a.auth_plain(&db, "alice", "pw", "1.2.3.4:5678")
            .await
            .unwrap();
        let err = a
            .auth_plain(&db, "alice", "bad", "1.2.3.4:5678")
            .await
            .unwrap_err();
        assert_eq!(auth_error_reason(&err), "Invalid credentials");
    }
    #[test]
    fn http_client_setup_failure_is_returned() {
        let builder = reqwest::Client::builder().user_agent("invalid\nheader");
        assert!(http_client_with_builder(builder).is_err());
    }
}
