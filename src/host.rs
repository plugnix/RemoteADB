//! The host role: keep adb alive and serve it to allowlisted clients.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::BTreeSet, io, str::FromStr, time::Duration};

use anyhow::Result;
use iroh::{EndpointId, SecretKey};
use tokio::{fs, time::sleep};
use tokio_util::sync::CancellationToken;

use crate::{
    Allowlist, Config, HostConfig, Paths, TunnelBuilder, adb::supervise_adb,
    keys::load_or_create_key,
};

const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything the host command takes from the command line. Unset values
/// fall back to the config file.
#[derive(Debug, Clone, Default)]
pub struct HostOptions {
    pub adb_port: Option<u16>,
    pub adb_path: Option<String>,
    pub no_adb: bool,
    pub allow: Vec<String>,
    pub ephemeral: bool,
    pub system: bool,
    pub relay_url: Vec<String>,
}

pub(crate) fn parse_ids<'a>(
    ids: impl IntoIterator<Item = &'a String>,
) -> Result<BTreeSet<EndpointId>> {
    ids.into_iter()
        .map(|id| {
            EndpointId::from_str(id.trim())
                .map_err(|e| anyhow::anyhow!("invalid endpoint ID '{id}': {e}"))
        })
        .collect()
}

async fn key_for(paths: &Paths, ephemeral: bool) -> Result<SecretKey> {
    if ephemeral {
        Ok(SecretKey::generate())
    } else {
        load_or_create_key(&paths.key).await
    }
}

/// Run the host until `shutdown` resolves.
pub async fn run_host(
    opts: HostOptions,
    shutdown: impl Future<Output = io::Result<()>>,
) -> Result<()> {
    let system = opts.system || self_runas::is_elevated();
    let paths = Paths::resolve(system)?;
    let config = if system {
        Config::load_at(&paths).await?
    } else {
        Config::load().await?
    };

    let secret_key = key_for(&paths, opts.ephemeral).await?;
    let adb_port = opts.adb_port.unwrap_or_else(|| config.adb_port());
    let adb_path = opts
        .adb_path
        .clone()
        .or_else(|| config.adb_path.clone())
        .unwrap_or_else(|| "adb".to_string());

    let flag_allow = parse_ids(&opts.allow)?;
    let mut allowed = parse_ids(&config.allowed_clients)?;
    allowed.extend(flag_allow.iter().copied());
    let allow = Allowlist::new(allowed.iter().copied());

    let tunnel = TunnelBuilder::new(secret_key)
        .relay_urls(opts.relay_url.iter().map(String::as_str))?
        .relay_urls(config.relay_urls.iter().map(String::as_str))?
        .with_host(HostConfig {
            adb_port,
            allow: allow.clone(),
        })
        .build()
        .await?;

    let cancel = CancellationToken::new();
    if !opts.no_adb {
        tokio::spawn(supervise_adb(adb_path.clone(), adb_port, cancel.clone()));
    }

    // Reload the allowlist whenever the config file changes, so
    // `remoteadb allow` takes effect without a restart.
    {
        let allow = allow.clone();
        let config_path = paths.config.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut last = fs::metadata(&config_path)
                .await
                .ok()
                .and_then(|m| m.modified().ok());
            loop {
                tokio::select! {
                    _ = sleep(Duration::from_secs(2)) => {}
                    _ = cancel.cancelled() => return,
                }
                let modified = fs::metadata(&config_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok());
                if modified == last {
                    continue;
                }
                last = modified;
                match Config::load_from(&config_path).await {
                    Ok(config) => match parse_ids(&config.allowed_clients) {
                        Ok(mut ids) => {
                            ids.extend(flag_allow.iter().copied());
                            allow.replace(ids.iter().copied());
                            tracing::info!("allowlist reloaded: {} client(s)", ids.len());
                        }
                        Err(err) => tracing::error!("allowlist not reloaded: {err:#}"),
                    },
                    Err(err) => tracing::error!("config not reloaded: {err:#}"),
                }
            }
        });
    }

    if !tunnel.wait_online(ONLINE_TIMEOUT).await {
        eprintln!("warning: could not reach the relay yet; clients may not be able to connect");
    }
    let ticket = tunnel.ticket();
    let id = tunnel.id();

    // Publish the ticket for `remoteadb service status` and for copying.
    if !opts.ephemeral
        && let Err(err) = publish_ticket(&paths, &id.to_string(), &ticket.to_string()).await
    {
        tracing::warn!("could not write ticket file: {err:#}");
    }

    println!("remoteadb host is running");
    println!();
    println!("  endpoint id: {id}");
    println!("  ticket:      {ticket}");
    println!(
        "  adb server:  127.0.0.1:{adb_port}{}",
        if opts.no_adb { "" } else { " (supervised)" }
    );
    if allow.is_empty() {
        println!();
        println!("  no clients are allowed yet. On the client run `remoteadb id`, then here:");
        println!(
            "    remoteadb allow{} <ENDPOINT_ID>",
            if system { " --system" } else { "" }
        );
    } else {
        println!("  allowed:     {} client(s)", allow.len());
    }
    println!();
    println!("  on the client: remoteadb connect {ticket}");

    shutdown.await?;
    println!("shutting down...");
    cancel.cancel();
    tunnel.close().await
}

async fn publish_ticket(paths: &Paths, id: &str, ticket: &str) -> Result<()> {
    fs::create_dir_all(&paths.dir).await?;
    fs::write(paths.dir.join("endpoint_id"), id).await?;
    fs::write(&paths.ticket, ticket).await?;
    #[cfg(unix)]
    {
        use std::{fs::Permissions, os::unix::fs::PermissionsExt};
        fs::set_permissions(&paths.dir, Permissions::from_mode(0o755)).await?;
        fs::set_permissions(paths.dir.join("endpoint_id"), Permissions::from_mode(0o644)).await?;
        fs::set_permissions(&paths.ticket, Permissions::from_mode(0o644)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_parsed() {
        let key = SecretKey::generate();
        let ids = vec![key.public().to_string()];
        assert_eq!(parse_ids(&ids).unwrap().len(), 1);
        assert!(parse_ids(&["nope".to_string()]).is_err());
    }
}
