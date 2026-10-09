//! The iroh endpoint and the TCP-to-QUIC bridge. Derived from
//! pigeons' tunnel.rs (https://github.com/n0-computer/pigeons) and modified:
//! endpoint discovery is disabled and a relay is mandatory, peers are dialed
//! from tickets, and the client reuses one QUIC connection per host instead of
//! dialing per TCP connection.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayUrl, SecretKey,
    endpoint::{Connection, RelayMode, presets},
    protocol::Router,
};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::{net::TcpListener, sync::Mutex, time::timeout as with_timeout};

use crate::protocol::{ADB_ALPN, AdbProtocol, Allowlist, bridge};

/// Host role: expose the local adb server to allowlisted clients.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub adb_port: u16,
    pub allow: Allowlist,
}

#[derive(Debug)]
pub struct TunnelBuilder {
    /// Set to serve the local adb server; leave unset for a client.
    pub host: Option<HostConfig>,
    /// Identity of this endpoint.
    pub secret_key: SecretKey,
    /// Private relays. At least one is required.
    pub relay_urls: Vec<RelayUrl>,
}

impl TunnelBuilder {
    pub fn new(secret_key: SecretKey) -> Self {
        Self {
            host: None,
            secret_key,
            relay_urls: vec![],
        }
    }

    pub fn relay_urls<'a>(mut self, urls: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        for url in urls {
            let parsed =
                RelayUrl::from_str(url).map_err(|e| anyhow!("invalid relay URL '{url}': {e}"))?;
            if !self.relay_urls.contains(&parsed) {
                self.relay_urls.push(parsed);
            }
        }
        Ok(self)
    }

    pub fn with_host(mut self, host: HostConfig) -> Self {
        self.host = Some(host);
        self
    }

    pub async fn build(self) -> Result<Tunnel> {
        if self.relay_urls.is_empty() {
            bail!(
                "no relay configured: pass --relay-url or set relay_urls in the config file. \
                 remoteadb does not use public relays."
            );
        }
        let relay_map: RelayMap = self.relay_urls.iter().cloned().collect();

        // presets::Minimal sets only the crypto provider: no address lookup
        // (n0 DNS or pkarr) is registered, so nothing about this endpoint is
        // published anywhere. Peers are dialed by ticket, which carries the
        // relay URL.
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(self.secret_key)
            .relay_mode(RelayMode::Custom(relay_map))
            .bind()
            .await?;
        tracing::info!("endpoint bound, id={}", endpoint.id());

        let mut router = Router::builder(endpoint.clone());
        if let Some(host) = &self.host {
            router = router.accept(
                ADB_ALPN,
                AdbProtocol::new(host.adb_port, host.allow.clone()),
            );
        }
        let router = router.spawn();

        Ok(Tunnel { router })
    }
}

#[derive(Debug, Clone)]
pub struct Tunnel {
    router: Router,
}

impl Tunnel {
    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint().id()
    }

    /// Wait until the home relay is connected, up to `timeout`. Returns false
    /// if the relay could not be reached in time.
    pub async fn wait_online(&self, timeout: Duration) -> bool {
        with_timeout(timeout, self.endpoint().online())
            .await
            .is_ok()
    }

    /// Ticket with this endpoint's ID and relay URLs only; direct addresses
    /// are left out and found by holepunching.
    pub fn ticket(&self) -> EndpointTicket {
        short_ticket(&self.endpoint().addr())
    }

    /// Serve a local TCP port, forwarding each connection to the remote
    /// host's adb server over one QUIC stream.
    pub async fn serve_adb_client(&self, remote: EndpointAddr, bind: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("failed to bind {bind}"))?;
        tracing::info!("listening on {}", listener.local_addr()?);
        let pool = ConnectionPool::new(self.endpoint().clone(), remote);
        loop {
            let (tcp, peer) = listener.accept().await?;
            tracing::debug!("accepted {peer}");
            let pool = pool.clone();
            tokio::spawn(async move {
                let conn = match pool.get().await {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!("connecting to host failed: {err:#}");
                        return;
                    }
                };
                let (send, recv) = match conn.open_bi().await {
                    Ok(streams) => streams,
                    Err(err) => {
                        tracing::error!("opening stream failed: {err}");
                        return;
                    }
                };
                if let Err(err) = bridge(tcp, send, recv).await {
                    tracing::debug!("stream ended with error: {err}");
                }
            });
        }
    }

    pub async fn close(&self) -> Result<()> {
        self.router.shutdown().await.context("shutting down router")
    }
}

/// One QUIC connection to the host, reused by every local TCP connection and
/// re-established when it drops.
#[derive(Debug, Clone)]
struct ConnectionPool {
    endpoint: Endpoint,
    remote: EndpointAddr,
    conn: Arc<Mutex<Option<Connection>>>,
}

impl ConnectionPool {
    fn new(endpoint: Endpoint, remote: EndpointAddr) -> Self {
        Self {
            endpoint,
            remote,
            conn: Default::default(),
        }
    }

    async fn get(&self) -> Result<Connection> {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_ref()
            && conn.close_reason().is_none()
        {
            return Ok(conn.clone());
        }
        tracing::info!("connecting to host {}", self.remote.id);
        let conn = self
            .endpoint
            .connect(self.remote.clone(), ADB_ALPN)
            .await
            .context("dialing host")?;
        tracing::info!("connected to host {}", self.remote.id);
        *guard = Some(conn.clone());
        Ok(conn)
    }
}

/// Reduce an address to its endpoint ID and relay URLs.
pub fn short_ticket(addr: &EndpointAddr) -> EndpointTicket {
    let mut short = EndpointAddr::new(addr.id);
    for relay_url in addr.relay_urls() {
        short = short.with_relay_url(relay_url.clone());
    }
    EndpointTicket::new(short)
}

/// Turn a ticket or a bare endpoint ID into an address to dial.
///
/// A bare ID has no relay information of its own, so the configured relays
/// are attached to it.
pub fn resolve_target(target: &str, relay_urls: &[RelayUrl]) -> Result<EndpointAddr> {
    let target = target.trim();
    if let Ok(ticket) = EndpointTicket::from_str(target) {
        let addr = ticket.endpoint_addr().clone();
        if addr.relay_urls().next().is_none() && relay_urls.is_empty() {
            bail!("ticket carries no relay URL and none is configured");
        }
        let mut addr = addr;
        for url in relay_urls {
            addr = addr.with_relay_url(url.clone());
        }
        return Ok(addr);
    }
    let id = EndpointId::from_str(target)
        .map_err(|e| anyhow!("'{target}' is neither a ticket nor an endpoint ID: {e}"))?;
    if relay_urls.is_empty() {
        bail!("a bare endpoint ID needs a relay: pass --relay-url or set relay_urls in the config");
    }
    let mut addr = EndpointAddr::new(id);
    for url in relay_urls {
        addr = addr.with_relay_url(url.clone());
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use std::slice;

    use super::*;

    #[test]
    fn short_ticket_drops_direct_addresses() {
        let key = SecretKey::generate();
        let relay = RelayUrl::from_str("https://relay.example").unwrap();
        let addr = EndpointAddr::new(key.public())
            .with_relay_url(relay.clone())
            .with_ip_addr("192.168.1.2:1234".parse().unwrap());
        let ticket = short_ticket(&addr);
        let parsed = EndpointTicket::from_str(&ticket.to_string()).unwrap();
        let addr = parsed.endpoint_addr();
        assert_eq!(addr.id, key.public());
        assert_eq!(addr.relay_urls().collect::<Vec<_>>(), vec![&relay]);
        assert_eq!(addr.ip_addrs().count(), 0);
    }

    #[test]
    fn resolve_target_accepts_bare_id_with_relay() {
        let key = SecretKey::generate();
        let relay = RelayUrl::from_str("https://relay.example").unwrap();
        let addr = resolve_target(&key.public().to_string(), slice::from_ref(&relay)).unwrap();
        assert_eq!(addr.id, key.public());
        assert_eq!(addr.relay_urls().collect::<Vec<_>>(), vec![&relay]);
    }

    #[test]
    fn resolve_target_rejects_bare_id_without_relay() {
        let key = SecretKey::generate();
        assert!(resolve_target(&key.public().to_string(), &[]).is_err());
    }

    #[test]
    fn resolve_target_rejects_garbage() {
        assert!(resolve_target("not a ticket", &[]).is_err());
    }
}
