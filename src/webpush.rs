//! Web Push notifications: message encryption (RFC 8291, aes128gcm), VAPID
//! authentication (RFC 8292) and delivery, compatible with
//! github.com/SherClockHolmes/webpush-go.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::Generate;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;

const RECORD_SIZE: u32 = 2048;

/// Returned when the push service reports the subscription has expired
/// (HTTP 404, RFC 8030 section 7.3).
#[derive(Debug)]
pub struct SubscriptionExpired;

impl std::fmt::Display for SubscriptionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Web Push subscription expired")
    }
}

impl std::error::Error for SubscriptionExpired {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Urgency {
    Normal,
    High,
}

impl Urgency {
    fn as_str(self) -> &'static str {
        match self {
            Urgency::Normal => "normal",
            Urgency::High => "high",
        }
    }
}

/// Generates a VAPID key pair, returning (private, public) keys encoded in
/// unpadded base64url.
pub fn generate_vapid_keys() -> (String, String) {
    let secret = SecretKey::generate_from_rng(&mut rand::rng());
    let public = secret.public_key().to_sec1_bytes();
    (
        URL_SAFE_NO_PAD.encode(secret.to_bytes()),
        URL_SAFE_NO_PAD.encode(public),
    )
}

/// Decodes a subscription key: standard base64 first, then URL-safe, with
/// padding added if needed.
fn decode_subscription_key(key: &str) -> Result<Vec<u8>> {
    let mut s = key.to_owned();
    let rem = s.len() % 4;
    if rem != 0 {
        s.push_str(&"=".repeat(4 - rem));
    }
    STANDARD
        .decode(&s)
        .or_else(|_| URL_SAFE.decode(&s))
        .map_err(|e| anyhow!("illegal base64 data: {e}"))
}

fn decode_vapid_key(key: &str) -> Result<Vec<u8>> {
    URL_SAFE
        .decode(key)
        .or_else(|_| URL_SAFE_NO_PAD.decode(key))
        .map_err(|e| anyhow!("illegal base64 data: {e}"))
}

fn hkdf_expand(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = vec![0u8; len];
    hk.expand(info, &mut okm).expect("valid HKDF length");
    okm
}

/// Encrypts a message for a subscription, producing an aes128gcm body with
/// a single record padded to the record size.
pub fn encrypt(message: &[u8], auth: &str, p256dh: &str) -> Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    rand::fill(&mut salt);
    encrypt_with(
        message,
        auth,
        p256dh,
        &salt,
        SecretKey::generate_from_rng(&mut rand::rng()),
    )
}

fn encrypt_with(
    message: &[u8],
    auth: &str,
    p256dh: &str,
    salt: &[u8; 16],
    local: SecretKey,
) -> Result<Vec<u8>> {
    let auth_secret = decode_subscription_key(auth)?;
    let dh = decode_subscription_key(p256dh)?;
    let remote = PublicKey::from_sec1_bytes(&dh)
        .map_err(|_| anyhow!("Unmarshal Error: Public key is not a valid point on the curve"))?;
    let local_public = local.public_key().to_sec1_bytes();

    let shared = p256::ecdh::diffie_hellman(local.to_nonzero_scalar(), remote.as_affine());
    let shared = shared.raw_secret_bytes();

    let mut prk_info = b"WebPush: info\x00".to_vec();
    prk_info.extend_from_slice(&dh);
    prk_info.extend_from_slice(&local_public);
    let ikm = hkdf_expand(&auth_secret, shared, &prk_info, 32);

    let cek = hkdf_expand(salt, &ikm, b"Content-Encoding: aes128gcm\x00", 16);
    let nonce = hkdf_expand(salt, &ikm, b"Content-Encoding: nonce\x00", 12);

    let mut record = salt.to_vec();
    record.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    record.push(local_public.len() as u8);
    record.extend_from_slice(&local_public);

    let record_length = RECORD_SIZE as usize - 16;
    let max_pad_len = record_length - record.len();
    let mut data = message.to_vec();
    data.push(0x02);
    if data.len() > max_pad_len {
        bail!("payload has exceeded the maximum length");
    }
    data.resize(max_pad_len, 0);

    let cipher = Aes128Gcm::new_from_slice(&cek).expect("valid AES key length");
    let nonce = Nonce::try_from(&nonce[..]).expect("valid nonce length");
    let ciphertext = cipher
        .encrypt(&nonce, data.as_slice())
        .map_err(|_| anyhow!("encryption failed"))?;
    record.extend_from_slice(&ciphertext);
    Ok(record)
}

/// Builds the VAPID Authorization header for an endpoint.
pub fn vapid_authorization(
    endpoint: &str,
    subscriber: &str,
    public_key: &str,
    private_key: &str,
) -> Result<String> {
    let u = crate::database::parse_url(endpoint)?;
    let subscriber = if subscriber.starts_with("https:") {
        subscriber.to_owned()
    } else {
        format!("mailto:{subscriber}")
    };
    let exp = chrono::Utc::now().timestamp() + 12 * 3600;
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"ES256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "aud": format!("{}://{}", u.scheme, u.host),
        "exp": exp,
        "sub": subscriber,
    });
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{header}.{claims}");

    let priv_bytes = decode_vapid_key(private_key)?;
    let mut padded = [0u8; 32];
    if priv_bytes.len() > 32 {
        bail!("invalid VAPID private key");
    }
    padded[32 - priv_bytes.len()..].copy_from_slice(&priv_bytes);
    let key = SigningKey::from_slice(&padded).map_err(|_| anyhow!("invalid VAPID private key"))?;
    let sig: Signature = key.sign(signing_input.as_bytes());
    let jwt = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));

    let pub_bytes = decode_vapid_key(public_key)?;
    Ok(format!(
        "vapid t={jwt}, k={}",
        URL_SAFE_NO_PAD.encode(pub_bytes)
    ))
}

/// Reports whether an IP address is internal and must not be contacted by
/// the Web Push client.
pub fn is_internal_ip(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_private()
                || v4.is_link_local()
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // unique local
                || (seg0 & 0xffc0) == 0xfe80 // link-local unicast
        }
    }
}

const INTERNAL_IP_ERROR: &str = "cannot connect to internal IP address";

/// A DNS resolver which refuses internal addresses.
struct ExternalResolver;

impl reqwest::dns::Resolve for ExternalResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| !is_internal_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(INTERNAL_IP_ERROR.into());
            }
            let iter: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

/// Fails if the URL's host is an internal IP address.
fn check_external_url(url: &reqwest::Url) -> Result<(), &'static str> {
    // The parsed URL's host is normalized: IPv4 in dotted-decimal form,
    // IPv6 in brackets
    let host = url.host_str().unwrap_or("");
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_internal_ip(ip)
    {
        return Err(INTERNAL_IP_ERROR);
    }
    Ok(())
}

/// Sends Web Push notifications with a fixed VAPID key pair.
pub struct Sender {
    pub vapid_public: String,
    vapid_private: String,
    client: reqwest::Client,
}

impl Sender {
    pub fn new(vapid_public: &str, vapid_private: &str) -> Sender {
        let tls = crate::auth::rustls_client_config();
        // Internal addresses are rejected: hostnames by the resolver, IP
        // literals in the endpoint and in redirects by check_external_url
        let redirect = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("stopped after 10 redirects")
            } else if let Err(e) = check_external_url(attempt.url()) {
                attempt.error(e)
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .dns_resolver(Arc::new(ExternalResolver))
            .redirect(redirect)
            .no_proxy()
            .user_agent("soju")
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client");
        Sender {
            vapid_public: vapid_public.to_owned(),
            vapid_private: vapid_private.to_owned(),
            client,
        }
    }

    /// Sends a notification. Fails with [`SubscriptionExpired`] if the
    /// subscription is gone.
    pub async fn send(
        &self,
        endpoint: &str,
        auth: &str,
        p256dh: &str,
        vapid_pub_key: &str,
        payload: &[u8],
        urgency: Urgency,
    ) -> Result<()> {
        if vapid_pub_key != self.vapid_public {
            bail!("unknown VAPID public key {vapid_pub_key:?}");
        }
        crate::database::parse_url(endpoint)?;
        // Parse like the HTTP client does: it normalizes hosts such as
        // "2130706433" or "127.1" to IP addresses
        let url = reqwest::Url::parse(endpoint)?;
        check_external_url(&url).map_err(|e| anyhow!(e))?;

        let body = encrypt(payload, auth, p256dh)?;
        let authorization = vapid_authorization(
            endpoint,
            "https://soju.im",
            &self.vapid_public,
            &self.vapid_private,
        )?;
        let resp = self
            .client
            .post(endpoint)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", (7 * 24 * 60 * 60).to_string())
            .header("Urgency", urgency.as_str())
            .header("Authorization", authorization)
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(SubscriptionExpired.into());
        } else if !status.is_success() {
            bail!("HTTP error: {status}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    /// Decrypts a message as a user agent would (RFC 8291 section 3.4).
    fn decrypt(body: &[u8], ua_secret: &SecretKey, auth: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
        assert_eq!(rs, RECORD_SIZE);
        let idlen = body[20] as usize;
        let as_public = &body[21..21 + idlen];
        let ciphertext = &body[21 + idlen..];
        let ua_public = ua_secret.public_key().to_sec1_bytes();
        let remote = PublicKey::from_sec1_bytes(as_public).unwrap();
        let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), remote.as_affine());
        let mut info = b"WebPush: info\x00".to_vec();
        info.extend_from_slice(&ua_public);
        info.extend_from_slice(as_public);
        let ikm = hkdf_expand(auth, shared.raw_secret_bytes(), &info, 32);
        let cek = hkdf_expand(salt, &ikm, b"Content-Encoding: aes128gcm\x00", 16);
        let nonce = hkdf_expand(salt, &ikm, b"Content-Encoding: nonce\x00", 12);
        let cipher = Aes128Gcm::new_from_slice(&cek).unwrap();
        let mut plain = cipher
            .decrypt(&Nonce::try_from(&nonce[..]).unwrap(), ciphertext)
            .unwrap();
        while plain.last() == Some(&0) {
            plain.pop();
        }
        assert_eq!(plain.pop(), Some(2));
        plain
    }

    #[test]
    fn encrypt_roundtrip() {
        let ua = SecretKey::generate_from_rng(&mut rand::rng());
        let auth = [9u8; 16];
        let p256dh = URL_SAFE_NO_PAD.encode(ua.public_key().to_sec1_bytes());
        let body = encrypt(
            b"@time=x PRIVMSG a :hi",
            &URL_SAFE_NO_PAD.encode(auth),
            &p256dh,
        )
        .unwrap();
        assert_eq!(body.len(), RECORD_SIZE as usize);
        assert_eq!(decrypt(&body, &ua, &auth), b"@time=x PRIVMSG a :hi");
        assert!(encrypt(&[b'x'; 3000], &URL_SAFE_NO_PAD.encode(auth), &p256dh).is_err());
    }

    #[test]
    fn vapid() {
        let (private, public) = generate_vapid_keys();
        assert_eq!(URL_SAFE_NO_PAD.decode(&public).unwrap().len(), 65);
        let h = vapid_authorization(
            "https://push.example.org/abc",
            "https://soju.im",
            &public,
            &private,
        )
        .unwrap();
        let rest = h.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, public);
        let parts: Vec<&str> = jwt.split('.').collect();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.org");
        assert_eq!(claims["sub"], "https://soju.im");
        let vk = VerifyingKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(&public).unwrap()).unwrap();
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }

    #[test]
    fn internal_ips() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "::1",
            "fe80::1",
            "fd00::1",
            "0.0.0.0",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_internal_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "2606:4700::1111"] {
            assert!(!is_internal_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn external_url() {
        // Hosts which the URL parser normalizes to internal IP addresses
        for u in [
            "http://127.0.0.1/x",
            "http://2130706433/x",
            "http://0x7f.1/x",
            "http://127.1/x",
            "https://[::1]:8443/x",
            "http://[::ffff:127.0.0.1]/x",
            "http://10.0.0.1/x",
        ] {
            let url = reqwest::Url::parse(u).unwrap();
            assert!(check_external_url(&url).is_err(), "{u}");
        }
        for u in ["https://push.example.org/x", "https://1.1.1.1/x"] {
            let url = reqwest::Url::parse(u).unwrap();
            assert!(check_external_url(&url).is_ok(), "{u}");
        }
    }
}
