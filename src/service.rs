//! Cross-platform service dispatch. Taken from pigeons
//! (https://github.com/n0-computer/pigeons) and modified: parameters carry the
//! adb port and a user-service flag, and the published endpoint ID was replaced
//! by a ticket that also carries the relay URL.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    env,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
};

use tokio::fs;

use crate::Paths;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use crate::service::linux::LinuxService;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use crate::service::macos::MacosService;

// Much of the windows module is invoked by the Windows Service Control Manager
// rather than our own code paths, so the compiler sees it as dead code.
#[cfg(target_os = "windows")]
#[allow(dead_code, reason = "invoked by the Windows Service Control Manager")]
mod windows;

#[cfg(target_os = "windows")]
pub(crate) use crate::service::windows::WindowsService;

/// Entry point for the Windows Service Control Manager: runs the host with
/// the machine-wide config until the service is stopped.
#[cfg(target_os = "windows")]
pub async fn run_windows_service(opts: crate::HostOptions) -> anyhow::Result<()> {
    WindowsService::run_service(ServiceParams {
        adb_port: opts.adb_port.unwrap_or(crate::config::DEFAULT_ADB_PORT),
        relay_url: opts.relay_url,
        binary_path: PathBuf::new(),
        user: false,
    })
    .await
}

#[derive(Debug, Clone)]
pub struct ServiceParams {
    pub adb_port: u16,
    pub relay_url: Vec<String>,
    pub binary_path: PathBuf,
    /// Linux only: install as a per-user systemd unit instead of a system one.
    pub user: bool,
}

/// Discover the absolute path of the running binary and check that it lives
/// somewhere a daemon can rely on. A user service accepts any path.
pub fn resolve_binary_path(user: bool) -> anyhow::Result<PathBuf> {
    #[cfg(not(unix))]
    let _ = user;
    let exe = env::current_exe()?;
    let resolved = exe.canonicalize()?;

    #[cfg(unix)]
    if !user {
        const SENSIBLE_PREFIXES: &[&str] = &[
            "/usr/local/bin",
            "/usr/bin",
            "/opt/homebrew/bin",
            "/opt/",
            "/usr/local/sbin",
            "/usr/sbin",
            "/var/usrlocal/bin/",
        ];

        let path_str = resolved
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("binary path is not valid UTF-8: {resolved:?}"))?;

        if !SENSIBLE_PREFIXES
            .iter()
            .any(|pfx| path_str.starts_with(pfx))
        {
            anyhow::bail!(
                "remoteadb binary is at {path_str}, which doesn't look like a permanent install location.\n\
                 Install it to one of ({}) before running service install, or use --user.",
                SENSIBLE_PREFIXES.join(", ")
            );
        }
    }

    tracing::info!("resolved remoteadb binary path: {}", resolved.display());
    Ok(resolved)
}

pub trait Service {
    fn install(service_params: ServiceParams) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn uninstall(user: bool) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn restart(user: bool) -> impl Future<Output = anyhow::Result<()>> + Send;
}

pub async fn install(service_params: ServiceParams) -> anyhow::Result<()> {
    tracing::info!(
        "installing service for os={}, adb_port={}, user={}",
        env::consts::OS,
        service_params.adb_port,
        service_params.user
    );
    match env::consts::OS {
        #[cfg(target_os = "linux")]
        "linux" => LinuxService::install(service_params).await,
        #[cfg(target_os = "macos")]
        "macos" => MacosService::install(service_params).await,
        #[cfg(target_os = "windows")]
        "windows" => WindowsService::install(service_params).await,
        _ => anyhow::bail!("service mode is only supported on linux, macos, and windows"),
    }
}

pub async fn uninstall(user: bool) -> anyhow::Result<()> {
    match env::consts::OS {
        #[cfg(target_os = "linux")]
        "linux" => LinuxService::uninstall(user).await,
        #[cfg(target_os = "macos")]
        "macos" => MacosService::uninstall(user).await,
        #[cfg(target_os = "windows")]
        "windows" => WindowsService::uninstall(user).await,
        _ => anyhow::bail!("service mode is only supported on linux, macos, and windows"),
    }
}

pub async fn restart(user: bool) -> anyhow::Result<()> {
    match env::consts::OS {
        #[cfg(target_os = "linux")]
        "linux" => LinuxService::restart(user).await,
        #[cfg(target_os = "macos")]
        "macos" => MacosService::restart(user).await,
        #[cfg(target_os = "windows")]
        "windows" => WindowsService::restart(user).await,
        _ => anyhow::bail!("service mode is only supported on linux, macos, and windows"),
    }
}

/// The endpoint ID and ticket a running host published, from the user's
/// directory first and the machine-wide one second.
pub async fn service_ticket() -> Option<(String, String)> {
    let mut candidates = vec![];
    if let Ok(user) = Paths::user() {
        candidates.push(user);
    }
    if let Ok(system) = Paths::system() {
        candidates.push(system);
    }
    for paths in candidates {
        let Ok(ticket) = fs::read_to_string(&paths.ticket).await else {
            continue;
        };
        let id = fs::read_to_string(paths.dir.join("endpoint_id"))
            .await
            .unwrap_or_default();
        return Some((id.trim().to_string(), ticket.trim().to_string()));
    }
    None
}

/// Print service logs to stdout.
pub fn service_log(user: bool) -> anyhow::Result<()> {
    match env::consts::OS {
        "macos" => {
            let path = Path::new("/var/log/remoteadb.log");
            if !path.exists() {
                anyhow::bail!(
                    "no log file found at /var/log/remoteadb.log; is the service installed?"
                );
            }
            let status = Command::new("cat").arg(path).status()?;
            if !status.success() {
                anyhow::bail!("failed to read log file (try running with sudo)");
            }
            Ok(())
        }
        "linux" => {
            let mut cmd = Command::new("journalctl");
            if user {
                cmd.arg("--user");
            }
            let status = cmd
                .args(["-u", "remoteadb.service", "--no-pager", "-n", "200"])
                .status()?;
            if !status.success() {
                anyhow::bail!("failed to read service journal");
            }
            Ok(())
        }
        other => anyhow::bail!("service log is not supported on {other}"),
    }
}
