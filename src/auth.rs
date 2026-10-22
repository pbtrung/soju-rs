//! Authentication backends.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use crate::config::{self, AuthDriver};
use crate::database::{Database, parse_url, split_host_port};

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
pub fn invalid_credentials(err: anyhow::Error) -> anyhow::Error {
    AuthError {
        internal: err,
        external_msg: "Invalid credentials".to_owned(),
    }
    .into()
}

/// Returns the user-friendly reason of an authentication failure.
pub fn auth_error_reason(err: &anyhow::Error) -> String {
    match err.downcast_ref::<AuthError>() {
        Some(e) => e.external_msg.clone(),
        None => "Authentication failed".to_owned(),
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(rustls_client_config())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client")
}

/// A TLS client configuration for HTTP/1.1 clients.
pub(crate) fn rustls_client_config() -> rustls::ClientConfig {
    let cfg = crate::tls::client_config("", None).expect("failed to build TLS config");
    let mut cfg = (*cfg).clone();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg
}

fn set_forwarded(req: reqwest::RequestBuilder, addr: &str) -> reqwest::RequestBuilder {
    if addr.is_empty() {
        return req;
    }
    let host = split_host_port(addr).map(|(h, _)| h).unwrap_or_default();
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
    pub fn internal() -> Authenticator {
        Authenticator {
            plain: Some(PlainAuthenticator::Internal),
            oauth_bearer: None,
        }
    }

    /// Builds the authenticator from config directives.
    pub async fn from_config(auths: &[config::Auth]) -> Result<Authenticator> {
        let mut out = Authenticator::default();
        for a in auths {
            let (plain, bearer) = match a.driver {
                AuthDriver::Internal => (Some(PlainAuthenticator::Internal), None),
                AuthDriver::Http => (
                    Some(PlainAuthenticator::Http {
                        url: a.source.clone(),
                        client: http_client(),
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
                        "failed to load authenticators: multiple plain authentication methods specified"
                    );
                }
                out.plain = Some(p);
            }
            if let Some(b) = bearer {
                if out.oauth_bearer.is_some() {
                    bail!(
                        "failed to load authenticators: multiple OAuth authentication methods specified"
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
    /// Checks credentials. remote_addr is the downstream address, passed to
    /// external services in Forwarded headers.
    pub async fn auth_plain(
        &self,
        db: &Arc<Database>,
        username: &str,
        password: &str,
        remote_addr: &str,
    ) -> Result<()> {
        match self {
            PlainAuthenticator::Internal => {
                let db = db.clone();
                let username = username.to_owned();
                let password = password.to_owned();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let Some(mut u) = db.get_user(&username)? else {
                        return Err(invalid_credentials(anyhow!(
                            "user not found: sql: no rows in result set"
                        )));
                    };
                    let upgraded = u.check_password(&password).map_err(invalid_credentials)?;
                    if upgraded {
                        db.store_user(&mut u)?;
                    }
                    Ok(())
                })
                .await?
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
                        "username mismatch (OAuth 2.0 server returned {effective:?})"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// OAuth 2.0 token introspection (RFC 7662), with server discovery (RFC
/// 8414).
pub struct OAuth2 {
    introspection_url: String,
    client_id: String,
    client_secret: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct OAuth2Server {
    #[serde(default)]
    issuer: String,
    #[serde(default)]
    introspection_endpoint: String,
    introspection_endpoint_auth_methods_supported: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct OAuth2Introspection {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    username: String,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Exactly two hex digits: from_str_radix would also accept a sign,
        // e.g. "%+f"
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = hex::decode(&b[i + 1..i + 3])
        {
            out.push(v[0]);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escapes a string like Go's url.QueryEscape.
pub fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl OAuth2 {
    pub async fn new(auth_url: &str) -> Result<OAuth2> {
        let u = parse_url(auth_url)
            .map_err(|e| anyhow!("failed to parse OAuth 2.0 server URL: {e}"))?;
        let (mut client_id, mut client_secret) = (String::new(), String::new());
        let rest = auth_url.split_once("://").map(|(_, r)| r).unwrap_or("");
        let authority = rest.split('/').next().unwrap_or("");
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            match userinfo.split_once(':') {
                Some((id, secret)) => {
                    client_id = percent_decode(id);
                    client_secret = percent_decode(secret);
                }
                None => client_id = percent_decode(userinfo),
            }
        }

        let path = format!("/.well-known/oauth-authorization-server{}", u.path)
            .trim_end_matches('/')
            .to_owned();
        let discovery_url = format!("{}://{}{}", u.scheme, u.host, path);
        let client = http_client();
        let server =
            tokio::time::timeout(Duration::from_secs(10), discover(&client, &discovery_url))
                .await
                .map_err(|_| anyhow!("OAuth 2.0 discovery failed: context deadline exceeded"))?
                .map_err(|e| anyhow!("OAuth 2.0 discovery failed: {e}"))?;

        if server.introspection_endpoint.is_empty() {
            bail!("OAuth 2.0 server doesn't support token introspection");
        }
        if let Some(methods) = &server.introspection_endpoint_auth_methods_supported {
            let supports_none = methods.iter().any(|m| m == "none");
            let supports_basic = methods.iter().any(|m| m == "client_secret_basic");
            if client_id.is_empty() && !supports_none {
                bail!("OAuth 2.0 server requires authentication for introspection");
            }
            if !client_id.is_empty() && !supports_basic {
                bail!(
                    "OAuth 2.0 server doesn't support Basic HTTP authentication for introspection"
                );
            }
        }

        Ok(OAuth2 {
            introspection_url: server.introspection_endpoint,
            client_id,
            client_secret,
            client,
        })
    }

    pub async fn auth_oauth_bearer(&self, token: &str, remote_addr: &str) -> Result<String> {
        let body = format!("token={}", query_escape(token));
        let mut req = self
            .client
            .post(&self.introspection_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body);
        req = set_forwarded(req, remote_addr);
        if !self.client_id.is_empty() {
            let creds = format!(
                "{}:{}",
                query_escape(&self.client_id),
                query_escape(&self.client_secret)
            );
            req = req.header("Authorization", format!("Basic {}", BASE64.encode(creds)));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("failed to send OAuth 2.0 introspection request: {e}"))?;
        if resp.status().as_u16() != 200 {
            bail!("OAuth 2.0 introspection error: {}", resp.status());
        }
        let data: OAuth2Introspection = resp
            .json()
            .await
            .map_err(|e| anyhow!("failed to decode OAuth 2.0 introspection response: {e}"))?;
        if !data.active {
            return Err(invalid_credentials(anyhow!("invalid access token")));
        }
        if data.username.is_empty() {
            // We really need the username here, otherwise an OAuth 2.0 user
            // can impersonate any other user.
            bail!("missing username in OAuth 2.0 introspection response");
        }
        Ok(data.username)
    }
}

async fn discover(client: &reqwest::Client, url: &str) -> Result<OAuth2Server> {
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| anyhow!("failed to send request: {e}"))?;
    if resp.status().as_u16() != 200 {
        bail!("OAuth 2.0 HTTP error: {}", resp.status());
    }
    let data: OAuth2Server = resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to decode response: {e}"))?;
    if data.issuer.is_empty() {
        bail!("missing issuer in response");
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::User;

    #[tokio::test]
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

    #[test]
    fn escape() {
        assert_eq!(query_escape("a b/c+d"), "a+b%2Fc%2Bd");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%3a%3A"), "::");
        assert_eq!(percent_decode("a%+fb%2"), "a%+fb%2");
    }

    #[tokio::test]
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
            client: http_client(),
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
}
