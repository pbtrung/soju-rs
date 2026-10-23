//! TLS configuration for listeners and upstream connections.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256, Sha512};

pub fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .clone()
}

fn sig_algs() -> WebPkiSupportedAlgorithms {
    provider().signature_verification_algorithms
}

/// Loads a PEM certificate chain and private key.
pub fn load_certified_key(cert_path: &str, key_path: &str) -> Result<CertifiedKey> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("failed to read {cert_path}"))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("failed to parse {cert_path}"))?;
    if certs.is_empty() {
        bail!("no certificate found in {cert_path}");
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("failed to read private key from {key_path}"))?;
    let key = provider()
        .key_provider
        .load_private_key(key)
        .map_err(|e| anyhow!("unsupported private key: {e}"))?;
    Ok(CertifiedKey::new(certs, key))
}

/// A server certificate which can be swapped at run time (on SIGHUP).
#[derive(Debug, Default)]
pub struct CertStore {
    key: RwLock<Option<Arc<CertifiedKey>>>,
}

impl CertStore {
    pub fn set(&self, key: CertifiedKey) {
        *self.key.write().unwrap() = Some(Arc::new(key));
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.key.read().unwrap().clone()
    }
}

/// Requests (but doesn't verify) a client certificate when client
/// certificate authentication is enabled, like Go's RequestClientCert.
#[derive(Debug)]
struct OptionalClientCert {
    enabled: Arc<AtomicBool>,
}

impl ClientCertVerifier for OptionalClientCert {
    fn offer_client_auth(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &sig_algs())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &sig_algs())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        sig_algs().supported_schemes()
    }
}

/// Builds a server config using the shared certificate store. alpn lists
/// the advertised protocols.
pub fn server_config(
    certs: Arc<CertStore>,
    client_cert_auth: Arc<AtomicBool>,
    alpn: &[&str],
) -> Result<Arc<rustls::ServerConfig>> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(OptionalClientCert {
            enabled: client_cert_auth,
        }))
        .with_cert_resolver(certs);
    cfg.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
    Ok(Arc::new(cfg))
}

fn root_store() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            Arc::new(roots)
        })
        .clone()
}

/// Checks the server certificate against a pinned fingerprint
/// ("sha-256:<hex>" or "sha-512:<hex>") instead of certificate authorities.
#[derive(Debug)]
struct PinnedCert {
    algo: String,
    fingerprint: String,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Only the leaf is checked: the handshake signature proves possession
        // of the leaf key, but nothing links the leaf to the other
        // certificates the server sent, so matching those would let anyone
        // append the pinned (public) certificate to their own chain.
        let remote = match self.algo.as_str() {
            "sha-512" => hex::encode(Sha512::digest(end_entity.as_ref())),
            "sha-256" => hex::encode(Sha256::digest(end_entity.as_ref())),
            _ => String::new(),
        };
        if !remote.is_empty() && remote == self.fingerprint {
            return Ok(ServerCertVerified::assertion());
        }
        // Fingerprints don't match, let's give the user a fingerprint they
        // can use to connect
        let remote = hex::encode(Sha512::digest(end_entity.as_ref()));
        Err(rustls::Error::General(format!(
            "the configured TLS certificate fingerprint doesn't match the server's - {remote}"
        )))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &sig_algs())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &sig_algs())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        sig_algs().supported_schemes()
    }
}

/// Builds a client config for an upstream connection.
///
/// certfp is the pinned fingerprint ("algo:hex"), if any. client_cert is a
/// DER certificate and PKCS#8 private key for SASL EXTERNAL.
pub fn client_config(
    certfp: &str,
    client_cert: Option<(&[u8], &[u8])>,
) -> Result<Arc<rustls::ClientConfig>> {
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?;
    let builder = if certfp.is_empty() {
        let verifier =
            WebPkiServerVerifier::builder_with_provider(root_store(), provider()).build()?;
        builder.with_webpki_verifier(verifier)
    } else {
        let (algo, fp) = certfp.split_once(':').unwrap_or(("", certfp));
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert {
                algo: algo.to_owned(),
                fingerprint: fp.to_owned(),
            }))
    };
    let mut cfg = match client_cert {
        Some((cert, key)) => builder
            .with_client_auth_cert(
                vec![CertificateDer::from(cert.to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
            )
            .map_err(|e| anyhow!("failed to parse private key: {e}"))?,
        None => builder.with_no_client_auth(),
    };
    cfg.alpn_protocols = vec![b"irc".to_vec()];
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn self_signed() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let (key, cert) = crate::certfp::generate_certfp("ed25519", 0).unwrap();
        (
            CertificateDer::from(cert),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
    }

    /// Runs a handshake against a server presenting chain (signed with key)
    /// while the client pins pinned. Returns whether it succeeded.
    async fn handshake(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        pinned: &CertificateDer<'_>,
        version: &'static rustls::SupportedProtocolVersion,
    ) -> bool {
        let server_cfg = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[version])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap();
        let certfp = format!("sha-256:{}", hex::encode(Sha256::digest(pinned.as_ref())));
        let mut client_cfg = (*client_config(&certfp, None).unwrap()).clone();
        client_cfg.alpn_protocols.clear();
        let (a, b) = tokio::io::duplex(64 * 1024);
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let server = tokio::spawn(async move {
            let mut s = acceptor.accept(a).await.ok()?;
            s.write_all(b"x").await.ok()?;
            s.flush().await.ok()
        });
        let name = ServerName::try_from("example.org").unwrap();
        let ok = match connector.connect(name, b).await {
            Ok(mut s) => {
                let mut buf = [0u8; 1];
                s.read_exact(&mut buf).await.is_ok()
            }
            Err(_) => false,
        };
        let _ = server.await;
        ok
    }

    #[tokio::test]
    async fn pinned_cert() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let (cert_a, key_a) = self_signed();
            let (cert_b, key_b) = self_signed();
            assert!(handshake(vec![cert_a.clone()], key_a, &cert_a, version).await);
            // An unrelated leaf followed by the pinned certificate must be
            // rejected
            assert!(
                !handshake(
                    vec![cert_b.clone(), cert_a.clone()],
                    key_b,
                    &cert_a,
                    version
                )
                .await
            );
        }
    }
}
