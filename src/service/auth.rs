//! BouncerServ certificate and SASL commands.

use anyhow::{Result, anyhow, bail};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use super::network::get_network_from_flag;
use super::{ServiceContext, no_args, parse_flags};
use crate::database as db;
use crate::flags::FlagSet;
use crate::user::User;

/// Maximum number of RSA key bits used when generating a new private key.
const MAX_RSA_BITS: i64 = 8192;

const DEVICE_CERTIFICATE_DISPLAY_BYTES: usize = 6;

fn send_certfp_fingerprints(ctx: &mut ServiceContext, cert: &[u8]) {
    ctx.print(format!(
        "SHA-1 fingerprint: {}",
        hex::encode(Sha1::digest(cert))
    ));
    ctx.print(format!(
        "SHA-256 fingerprint: {}",
        hex::encode(Sha256::digest(cert))
    ));
    ctx.print(format!(
        "SHA-512 fingerprint: {}",
        hex::encode(Sha512::digest(cert))
    ));
}

pub(super) fn handle_certfp_generate(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(
        &FlagSet::new()
            .string("network")
            .string("key-type")
            .int("bits"),
        params,
    )?;
    no_args(&p)?;
    let key_type = p.string_or("key-type", "rsa");
    let bits = p.get_int("bits").unwrap_or(3072);
    if bits <= 0 || bits > MAX_RSA_BITS {
        bail!("invalid value for -bits");
    }
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let (key, cert) = crate::certfp::generate_certfp(&key_type, bits as usize)?;
    let net = &mut u.networks[ni];
    net.record.sasl.external_cert = Some(cert.clone());
    net.record.sasl.external_key = Some(key);
    net.record.sasl.mechanism = "EXTERNAL".to_owned();
    net.sync_snapshot();
    ctx.srv.db.store_network(u.record.id, &mut net.record)?;
    ctx.print("certificate generated");
    send_certfp_fingerprints(ctx, &cert);
    Ok(())
}

pub(super) fn handle_certfp_fingerprints(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let sasl = &u.networks[ni].record.sasl;
    if sasl.mechanism != "EXTERNAL" {
        bail!("CertFP not set up");
    }
    let cert = sasl.external_cert.clone().unwrap_or_default();
    send_certfp_fingerprints(ctx, &cert);
    Ok(())
}

pub(super) fn handle_sasl_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let net = &u.networks[ni];
    match net.record.sasl.mechanism.as_str() {
        "PLAIN" => ctx.print(format!(
            "SASL PLAIN enabled with username {:?}",
            net.record.sasl.plain_username
        )),
        "EXTERNAL" => ctx.print("SASL EXTERNAL (CertFP) enabled"),
        "" => ctx.print("SASL is disabled"),
        _ => {}
    }
    match net.upstream() {
        Some(uc) if !uc.account.is_empty() => ctx.print(format!(
            "Authenticated on upstream network with account {:?}",
            uc.account
        )),
        Some(_) => ctx.print("Unauthenticated on upstream network"),
        None => ctx.print("Disconnected from upstream network"),
    }
    Ok(())
}

pub(super) fn handle_sasl_set_plain(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    if p.args.len() != 2 {
        bail!("expected exactly 2 arguments");
    }
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let net = &mut u.networks[ni];
    net.record.sasl.plain_username = p.args[0].clone();
    net.record.sasl.plain_password = p.args[1].clone();
    net.record.sasl.mechanism = "PLAIN".to_owned();
    ctx.srv.db.store_network(u.record.id, &mut net.record)?;
    net.sync_snapshot();
    ctx.print("credentials saved");
    Ok(())
}

pub(super) fn handle_sasl_reset(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let ni = get_network_from_flag(u, ctx, p.get("network").unwrap_or(""))?;
    let net = &mut u.networks[ni];
    net.record.sasl = db::Sasl::default();
    net.sync_snapshot();
    ctx.srv.db.store_network(u.record.id, &mut net.record)?;
    ctx.print("credentials reset");
    Ok(())
}

fn colon_hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub(super) fn handle_device_cert_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if !params.is_empty() {
        bail!("expected no argument");
    }
    let certs = ctx
        .srv
        .db
        .list_device_certificates(u.record.id)
        .map_err(|e| anyhow!("could not get device certificates: {e}"))?;
    if certs.is_empty() {
        ctx.print("No device certificate configured.");
        return Ok(());
    }
    for cert in certs {
        let n = DEVICE_CERTIFICATE_DISPLAY_BYTES.min(cert.fingerprint.len());
        let last_used = cert
            .last_used
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "0001-01-01 00:00:00".to_owned());
        ctx.print(format!(
            "{} [{}] [{}] [{}]",
            colon_hex(&cert.fingerprint[..n]),
            cert.last_ip,
            cert.label,
            last_used
        ));
    }
    Ok(())
}

pub(super) fn handle_device_cert_create(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if !ctx.srv.config().basic.client_cert_auth {
        bail!("client certification authentication is disabled");
    }
    let p = parse_flags(
        &FlagSet::new().string("fingerprint").string("label"),
        params,
    )?;
    no_args(&p)?;
    let mut fingerprint = p.string_or("fingerprint", "");
    let label = p.string_or("label", "");
    if fingerprint.is_empty()
        && let Some(cert) = &ctx.peer_cert
    {
        fingerprint = colon_hex(&Sha512::digest(cert));
    }
    if fingerprint.is_empty() {
        bail!("flag -fingerprint is required when no TLS client certificate is used");
    }
    if label.is_empty() {
        bail!("flag -label is required");
    }
    let h = match hex::decode(fingerprint.replace(':', "")) {
        Ok(h) if h.len() == 64 => h,
        _ => bail!("the certificate fingerprint must be hex-encoded SHA-512"),
    };
    let mut cert = db::DeviceCertificate {
        label,
        fingerprint: h,
        ..Default::default()
    };
    cert.mark_used(ctx.remote_ip.map(|ip| std::net::SocketAddr::new(ip, 0)));
    match ctx.srv.db.store_device_certificate(u.record.id, &mut cert) {
        Ok(()) => {}
        Err(e) if e.is::<db::DuplicateDeviceCertificate>() => {
            bail!("this device certificate fingerprint already exists")
        }
        Err(e) => bail!("could not create device certificate: {e}"),
    }
    ctx.print(format!("created device certificate {fingerprint:?}"));
    Ok(())
}

pub(super) fn handle_device_cert_delete(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let fingerprint = &params[0];
    let h = match hex::decode(fingerprint.replace(':', "")) {
        Ok(h) if h.len() <= 64 && h.len() >= DEVICE_CERTIFICATE_DISPLAY_BYTES => h,
        _ => bail!(
            "the certificate fingerprint must be a hex-encoded SHA-512 hash, or a prefix of it"
        ),
    };
    let certs = ctx
        .srv
        .db
        .list_device_certificates(u.record.id)
        .map_err(|e| anyhow!("could not get device certificate: {e}"))?;
    let mut found: Option<db::DeviceCertificate> = None;
    for cert in certs {
        if cert.fingerprint.starts_with(&h) {
            if found.is_some() {
                bail!(
                    "multiple device certificates match this fingerprint prefix; pass the full fingerprint"
                );
            }
            found = Some(cert);
        }
    }
    let Some(c) = found else {
        bail!("no device certificate found for fingerprint {fingerprint:?}");
    };
    ctx.srv
        .db
        .delete_device_certificate(u.record.id, &c.fingerprint)
        .map_err(|e| anyhow!("could not delete device certificate: {e}"))?;
    ctx.print(format!("deleted device certificate {fingerprint:?}"));
    Ok(())
}
