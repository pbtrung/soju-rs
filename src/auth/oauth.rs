//! OAuth metadata trust, bounded responses and token introspection.

use std::borrow::Cow;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, de::DeserializeOwned};

use super::{invalid_credentials, percent_decode, query_escape, set_forwarded};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// OAuth token introspection with exact configured-issuer binding. Redirects
/// are rejected and every decoded metadata/introspection body is byte-bounded.
pub struct OAuth2 {
    introspection_url: String,
    client_id: String,
    client_secret: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(default)]
    issuer: String,
    #[serde(default)]
    introspection_endpoint: String,
    introspection_endpoint_auth_methods_supported: Option<Vec<String>>,
}
#[derive(Deserialize)]
struct Introspection {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    username: String,
}

struct Issuer {
    expected: String,
    discovery: String,
    parsed: reqwest::Url,
    insecure_loopback: bool,
    client_id: String,
    client_secret: String,
}
impl Issuer {
    fn parse(source: &str) -> Result<Self> {
        if source
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
            || source.contains('\\')
        {
            bail!("OAuth issuer URL contains disallowed whitespace, controls or backslashes");
        }
        let (actual, insecure_loopback) = source
            .strip_prefix("http+insecure://")
            .map_or((Cow::Borrowed(source), false), |rest| {
                (Cow::Owned(format!("http://{rest}")), true)
            });
        let parsed = reqwest::Url::parse(&actual)
            .map_err(|error| anyhow!(error).context("invalid OAuth issuer URL"))?;
        if parsed.query().is_some() || parsed.fragment().is_some() || parsed.host_str().is_none() {
            bail!("OAuth issuer must have a host and no query or fragment");
        }
        if insecure_loopback {
            let loopback = parsed
                .host_str()
                .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback());
            if parsed.scheme() != "http" || !loopback {
                bail!("insecure OAuth is restricted to literal loopback addresses");
            }
        } else if parsed.scheme() != "https" {
            bail!(
                "OAuth issuer requires HTTPS; local testing requires http+insecure with a literal loopback address"
            );
        }
        let client_id = percent_decode(parsed.username());
        let client_secret = parsed.password().map_or_default(percent_decode);
        if client_id.is_empty() && !client_secret.is_empty() {
            bail!("OAuth client secret requires a client ID");
        }
        // Preserve the configured code points/escaping/trailing slash for the
        // RFC 8414 equality check; URL serialization would normalize identity.
        let (scheme, rest) = actual
            .split_once("://")
            .ok_or_else(|| anyhow!("missing OAuth issuer scheme"))?;
        let (authority, path) = rest
            .split_once('/')
            .map_or((rest, String::new()), |(authority, path)| {
                (authority, format!("/{path}"))
            });
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host.is_empty() {
            bail!("OAuth issuer requires an explicit URL authority");
        }
        let expected = format!("{scheme}://{host}{path}");
        let discovery = format!(
            "{scheme}://{host}/.well-known/oauth-authorization-server{}",
            path.trim_end_matches('/')
        );
        Ok(Self {
            expected,
            discovery,
            parsed,
            insecure_loopback,
            client_id,
            client_secret,
        })
    }

    fn endpoint(&self, value: &str) -> Result<reqwest::Url> {
        let url = reqwest::Url::parse(value)
            .map_err(|error| anyhow!(error).context("invalid OAuth introspection endpoint"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
        {
            bail!("OAuth introspection endpoint must have a host and no userinfo or fragment");
        }
        if self.insecure_loopback {
            if url.origin() != self.parsed.origin() {
                bail!("insecure OAuth introspection must use the issuer's loopback origin");
            }
        } else if url.scheme() != "https" {
            bail!("OAuth introspection endpoint requires HTTPS");
        }
        Ok(url)
    }
}

fn request_error(error: reqwest::Error) -> anyhow::Error {
    // URLs may contain provider query credentials; none belong in auth logs.
    anyhow!(error.without_url()).context("OAuth HTTP request failed")
}

async fn bounded_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    let limit = u64::try_from(MAX_RESPONSE_BYTES)?;
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("OAuth response exceeds the 64-KiB limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            bail!("OAuth response exceeds the 64-KiB limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        // Serde data errors can quote response values, including echoed tokens.
        anyhow!(
            "invalid OAuth JSON response ({:?}, line {}, column {})",
            error.classify(),
            error.line(),
            error.column()
        )
    })
}

impl OAuth2 {
    /// Loads bounded metadata and verifies its exact issuer before using its
    /// introspection endpoint. `http+insecure://` is an explicit local-test
    /// extension restricted to a literal loopback issuer and same-origin HTTP.
    ///
    /// # Errors
    /// Returns invalid URL/policy, TLS, HTTP, metadata binding or decoding errors.
    pub async fn new(source: &str) -> Result<Self> {
        let issuer = Issuer::parse(source)?;
        let mut builder = reqwest::Client::builder()
            .use_preconfigured_tls(super::rustls_client_config()?)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT);
        if issuer.insecure_loopback {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(request_error)?;
        let response = client
            .get(&issuer.discovery)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(request_error)?;
        if response.status() != reqwest::StatusCode::OK {
            bail!("OAuth discovery HTTP status {}", response.status());
        }
        let metadata: Metadata = bounded_json(response).await?;
        if metadata.issuer.is_empty() {
            bail!("missing OAuth issuer in metadata");
        }
        if metadata.issuer != issuer.expected {
            bail!("OAuth metadata issuer does not match the configured issuer");
        }
        if metadata.introspection_endpoint.is_empty() {
            bail!("OAuth server does not support token introspection");
        }
        let endpoint = issuer.endpoint(&metadata.introspection_endpoint)?;
        if let Some(methods) = metadata.introspection_endpoint_auth_methods_supported {
            if issuer.client_id.is_empty() && !methods.iter().any(|method| method == "none") {
                bail!("OAuth server requires introspection client authentication");
            }
            if !issuer.client_id.is_empty()
                && !methods.iter().any(|method| method == "client_secret_basic")
            {
                bail!("OAuth server does not support Basic introspection authentication");
            }
        }
        Ok(Self {
            introspection_url: endpoint.into(),
            client_id: issuer.client_id,
            client_secret: issuer.client_secret,
            client,
        })
    }

    /// Introspects a token without logging its body, credentials or provider
    /// response values. An inactive token remains an invalid-credentials error.
    ///
    /// # Errors
    /// Returns HTTP/decoding failures, inactive tokens or missing usernames.
    pub async fn auth_oauth_bearer(&self, token: &str, remote_addr: &str) -> Result<String> {
        let body = format!("token={}", query_escape(token));
        let mut request = self
            .client
            .post(&self.introspection_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body);
        request = set_forwarded(request, remote_addr);
        if !self.client_id.is_empty() {
            let credentials = format!(
                "{}:{}",
                query_escape(&self.client_id),
                query_escape(&self.client_secret)
            );
            request = request.header(
                "Authorization",
                format!("Basic {}", BASE64.encode(credentials)),
            );
        }
        let response = request.send().await.map_err(request_error)?;
        if response.status() != reqwest::StatusCode::OK {
            bail!("OAuth introspection HTTP status {}", response.status());
        }
        let data: Introspection = bounded_json(response).await?;
        if !data.active {
            return Err(invalid_credentials(anyhow!("invalid access token")));
        }
        if data.username.is_empty() {
            bail!("missing username in OAuth introspection response");
        }
        Ok(data.username)
    }
}

#[cfg(test)]
mod tests;
