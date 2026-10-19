//! Self-signed client certificate generation for SASL EXTERNAL.

use anyhow::{Result, anyhow, bail};
use aws_lc_rs::encoding::AsDer;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P521_SHA512, PKCS_ED25519, PKCS_RSA_SHA256, SerialNumber,
};
use rustls::pki_types::PrivatePkcs8KeyDer;

/// Generates a private key and a self-signed certificate. Returns the
/// PKCS#8 private key and the certificate, both DER-encoded.
///
/// key_type is "rsa", "ecdsa" (NIST P-521) or "ed25519". bits is the RSA
/// key size: 2048, 3072, 4096 or 8192.
pub fn generate_certfp(key_type: &str, bits: usize) -> Result<(Vec<u8>, Vec<u8>)> {
    let key = match key_type {
        "rsa" => {
            let size = match bits {
                2048 => aws_lc_rs::rsa::KeySize::Rsa2048,
                3072 => aws_lc_rs::rsa::KeySize::Rsa3072,
                4096 => aws_lc_rs::rsa::KeySize::Rsa4096,
                8192 => aws_lc_rs::rsa::KeySize::Rsa8192,
                _ => bail!(
                    "unsupported RSA key size {bits} (supported sizes: 2048, 3072, 4096, 8192)"
                ),
            };
            let kp = aws_lc_rs::rsa::KeyPair::generate(size)
                .map_err(|_| anyhow!("failed to generate RSA key"))?;
            let der = kp
                .as_der()
                .map_err(|_| anyhow!("failed to encode RSA key"))?;
            let der = PrivatePkcs8KeyDer::from(der.as_ref().to_vec());
            KeyPair::from_pkcs8_der_and_sign_algo(&der, &PKCS_RSA_SHA256)?
        }
        "ecdsa" => KeyPair::generate_for(&PKCS_ECDSA_P521_SHA512)?,
        "ed25519" => KeyPair::generate_for(&PKCS_ED25519)?,
        // Same error as upstream, which fails to marshal a nil key
        _ => bail!("x509: unknown key type while marshaling PKCS#8: <nil>"),
    };

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "soju auto-generated certificate");
    params.distinguished_name = dn;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    // Let's make a fair assumption nobody will use the same cert for more
    // than 20 years...
    params.not_after = now + time::Duration::days(365 * 20);
    let mut serial = [0u8; 16];
    rand::fill(&mut serial);
    serial[0] &= 0x7f;
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    params.key_usages = vec![
        KeyUsagePurpose::KeyEncipherment,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let cert = params.self_signed(&key)?;
    Ok((key.serialize_der(), cert.der().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_all() {
        for (kt, bits) in [("ecdsa", 0), ("ed25519", 0), ("rsa", 2048)] {
            let (key, cert) = generate_certfp(kt, bits).unwrap();
            assert!(!key.is_empty() && !cert.is_empty());
            // The pair must be usable as a TLS client certificate
            crate::tls::client_config("", Some((&cert, &key))).unwrap();
        }
        assert!(generate_certfp("rsa", 1000).is_err());
        assert!(generate_certfp("dsa", 0).is_err());
    }
}
