//! Load the runtime configuration, MOTD, icon and authentication backends.

use super::ServerConfig;
use crate::auth::Authenticator;
use crate::config;
use crate::database as db;
use anyhow::{Result, anyhow, bail};

/// Builds the run-time configuration from the config file.
pub async fn load_server_config(raw: &config::Server) -> Result<ServerConfig> {
    // A hostname without a dot can confuse clients
    if !raw.basic.hostname.contains('.') {
        eprintln!(
            "{} warning: hostname {:?} is not a fully qualified domain name",
            chrono::Local::now().format("%Y/%m/%d %H:%M:%S"),
            raw.basic.hostname
        );
    }

    let mut motd = String::new();
    if !raw.motd_path.is_empty() {
        motd = std::fs::read_to_string(&raw.motd_path)
            .map_err(|e| anyhow!("failed to load MOTD: {e}"))?;
        if let Some(m) = motd.strip_suffix('\n') {
            motd = m.to_owned();
        }
    }

    let (mut icon_url, mut icon_path) = (String::new(), String::new());
    if raw.icon.contains("://") {
        let u = db::parse_url(&raw.icon).map_err(|e| anyhow!("invalid icon URL: {e}"))?;
        if u.scheme != "https" {
            bail!("unsupported icon URL scheme: {}", u.scheme);
        }
        icon_url = raw.icon.clone();
    } else if !raw.icon.is_empty() {
        std::fs::metadata(&raw.icon).map_err(|e| anyhow!("failed to load icon: {e}"))?;
        icon_path = raw.icon.clone();
    }

    let auth = Authenticator::from_config(&raw.auth).await?;

    Ok(ServerConfig {
        basic: raw.basic.clone(),
        icon_path,
        icon_url,
        motd,
        auth,
    })
}
