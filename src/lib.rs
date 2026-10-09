//! remoteadb: reach the adb server on another machine over an iroh tunnel.
//!
//! The host side (`remoteadb host`) keeps a local adb server alive and
//! accepts QUIC connections from an allowlist of client endpoint IDs. Each
//! bidirectional stream on such a connection is piped to one TCP connection
//! on the local adb server port.
//!
//! The client side (`remoteadb connect`) listens on a local TCP port and
//! turns each accepted connection into one stream to the host. The standard
//! `adb` client then talks to the remote server through
//! `ADB_SERVER_SOCKET=tcp:127.0.0.1:<port>`.
//!
//! Based on n0-computer's pigeons (MIT OR Apache-2.0):
//! https://github.com/n0-computer/pigeons
//! See NOTICE for what was derived and what changed.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

mod adb;
mod config;
mod host;
mod keys;
mod protocol;
mod service;
mod tunnel;

pub use adb::{adb_listening, supervise_adb};
pub use config::{Config, Paths, SavedHost};
pub use host::{HostOptions, run_host};
pub use keys::load_or_create_key;
pub use protocol::{ADB_ALPN, Allowlist};
#[cfg(target_os = "windows")]
pub use service::run_windows_service;
pub use service::{
    Service, ServiceParams, install as install_service, resolve_binary_path,
    restart as restart_service, service_log, service_ticket, uninstall as uninstall_service,
};
pub use tunnel::{HostConfig, Tunnel, TunnelBuilder, resolve_target, short_ticket};
