//! The adb forwarding protocol. The flushing copy loop comes from
//! pigeons (https://github.com/n0-computer/pigeons); the allowlist gate, the
//! stream-per-connection accept loop and the half-close handling are new.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::BTreeSet,
    error, fmt, io,
    sync::{Arc, RwLock},
};

use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

/// ALPN for adb server forwarding. Each bidirectional stream carries one TCP
/// connection to the host's adb server.
pub const ADB_ALPN: &[u8] = b"/remoteadb/adb/1";

/// The set of client endpoint IDs a host accepts. Shared with a reloader so
/// edits to the config take effect without restarting the host.
#[derive(Debug, Clone, Default)]
pub struct Allowlist(Arc<RwLock<BTreeSet<EndpointId>>>);

impl Allowlist {
    pub fn new(ids: impl IntoIterator<Item = EndpointId>) -> Self {
        Self(Arc::new(RwLock::new(ids.into_iter().collect())))
    }

    pub fn is_allowed(&self, id: &EndpointId) -> bool {
        self.0.read().expect("allowlist poisoned").contains(id)
    }

    pub fn replace(&self, ids: impl IntoIterator<Item = EndpointId>) {
        *self.0.write().expect("allowlist poisoned") = ids.into_iter().collect();
    }

    pub fn len(&self) -> usize {
        self.0.read().expect("allowlist poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug)]
struct NotAllowed(EndpointId);

impl fmt::Display for NotAllowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "endpoint {} is not in the allowlist", self.0)
    }
}

impl error::Error for NotAllowed {}

/// Host-side handler: rejects peers that are not allowlisted, then pipes
/// every stream to a fresh TCP connection on the adb server.
#[derive(Debug, Clone)]
pub(crate) struct AdbProtocol {
    adb_port: u16,
    allow: Allowlist,
}

impl AdbProtocol {
    pub(crate) fn new(adb_port: u16, allow: Allowlist) -> Self {
        Self { adb_port, allow }
    }
}

impl ProtocolHandler for AdbProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // The peer's ID is only known once the TLS handshake has completed,
        // so this is the earliest point to reject it. No application data
        // has been exchanged yet.
        let id = connection.remote_id();
        if !self.allow.is_allowed(&id) {
            tracing::warn!("rejected connection from {id}: not in allowlist");
            connection.close(1u8.into(), b"not allowed");
            return Err(AcceptError::from_err(NotAllowed(id)));
        }
        tracing::info!("client {id} connected");
        loop {
            let (send, recv) = match connection.accept_bi().await {
                Ok(streams) => streams,
                Err(err) => {
                    tracing::info!("client {id} disconnected: {err}");
                    break;
                }
            };
            let target = format!("127.0.0.1:{}", self.adb_port);
            tokio::spawn(async move {
                let tcp = match TcpStream::connect(&target).await {
                    Ok(tcp) => tcp,
                    Err(err) => {
                        tracing::error!("adb server at {target} unreachable: {err}");
                        return;
                    }
                };
                if let Err(err) = bridge(tcp, send, recv).await {
                    tracing::debug!("stream from {id} ended with error: {err}");
                }
            });
        }
        Ok(())
    }
}

/// Pipe a TCP stream and a QUIC stream pair into each other.
///
/// Each direction propagates a clean end of stream to the other side (QUIC
/// finish, TCP shutdown) so the adb protocol's half-closes survive the trip.
/// An error on either direction aborts the other.
pub(crate) async fn bridge(
    tcp: TcpStream,
    mut send: SendStream,
    mut recv: RecvStream,
) -> anyhow::Result<()> {
    tcp.set_nodelay(true).ok();
    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    let mut to_quic = tokio::spawn(async move {
        copy_flush(&mut tcp_read, &mut send).await?;
        send.finish()?;
        // Wait for the peer to acknowledge, otherwise dropping the stream may
        // reset it before the final bytes are delivered.
        let _ = send.stopped().await;
        anyhow::Ok(())
    });
    let mut to_tcp = tokio::spawn(async move {
        copy_flush(&mut recv, &mut tcp_write).await?;
        tcp_write.shutdown().await?;
        anyhow::Ok(())
    });

    tokio::select! {
        res = &mut to_quic => {
            match res {
                Ok(Ok(())) => { let _ = to_tcp.await; Ok(()) }
                Ok(Err(err)) => { to_tcp.abort(); Err(err) }
                Err(err) => { to_tcp.abort(); Err(err.into()) }
            }
        }
        res = &mut to_tcp => {
            match res {
                Ok(Ok(())) => { let _ = to_quic.await; Ok(()) }
                Ok(Err(err)) => { to_quic.abort(); Err(err) }
                Err(err) => { to_quic.abort(); Err(err.into()) }
            }
        }
    }
}

/// Copy data from reader to writer, flushing after every write so interactive
/// traffic such as `adb shell` keystrokes is forwarded immediately.
async fn copy_flush<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 32 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(total);
        }
        writer.write_all(&buf[..n]).await?;
        writer.flush().await?;
        total += n as u64;
    }
}
