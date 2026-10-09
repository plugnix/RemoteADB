//! systemd service backend. Taken from pigeons
//! (https://github.com/n0-computer/pigeons) and modified: the unit is written
//! from a template in Rust rather than a shell script, it starts the remoteadb
//! host, and a per-user unit path was added so adb can run as the logged-in
//! user.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::Context;
use tokio::fs;

use crate::{Service, ServiceParams};

#[derive(Debug, Clone)]
pub(crate) struct LinuxService;

impl Service for LinuxService {
    async fn install(service_params: ServiceParams) -> anyhow::Result<()> {
        if service_params.user {
            return Self::install_user(service_params).await;
        }
        let path = Self::init_install_script(service_params)?;
        tracing::debug!("running install script: {}", path.display());
        run_sh(&path, "install")
    }

    async fn uninstall(user: bool) -> anyhow::Result<()> {
        if user {
            return Self::uninstall_user().await;
        }
        let path = Self::init_uninstall_script()?;
        tracing::debug!("running uninstall script: {}", path.display());
        run_sh(&path, "uninstall")
    }

    async fn restart(user: bool) -> anyhow::Result<()> {
        let status = systemctl(user)
            .args(["restart", "remoteadb.service"])
            .status()?;
        if !status.success() {
            anyhow::bail!("systemctl restart failed with exit code: {}", status);
        }
        Ok(())
    }
}

fn systemctl(user: bool) -> Command {
    let mut cmd = Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

fn run_sh(path: &PathBuf, what: &str) -> anyhow::Result<()> {
    let status = Command::new("sh")
        .arg(path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        anyhow::bail!("{what} script failed with exit code: {}", status);
    }
    Ok(())
}

fn relay_args(service_params: &ServiceParams) -> String {
    let mut relay_args = String::new();
    for url in &service_params.relay_url {
        relay_args.push_str(&format!(" --relay-url {url}"));
    }
    relay_args
}

fn unit_file(service_params: &ServiceParams, system: bool) -> anyhow::Result<String> {
    let binary = service_params
        .binary_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("binary path is not valid UTF-8"))?;
    let wanted_by = if system {
        "multi-user.target"
    } else {
        "default.target"
    };
    let system_flag = if system { " --system" } else { "" };
    Ok(format!(
        "[Unit]\n\
         Description=remoteadb host (adb over iroh)\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment=RUST_LOG=remoteadb=info\n\
         ExecStart={binary} host{system_flag} --adb-port {}{}\n\
         Restart=on-failure\n\
         RestartSec=3s\n\
         \n\
         [Install]\n\
         WantedBy={wanted_by}\n",
        service_params.adb_port,
        relay_args(service_params)
    ))
}

impl LinuxService {
    const UNINSTALL_SH_BYTES: &str = include_str!("../../service/uninstall_linux.sh");

    fn init_install_script(service_params: ServiceParams) -> anyhow::Result<PathBuf> {
        use std::io::Write as _;

        let unit = unit_file(&service_params, true)?;
        let script = format!(
            "set -e\n\
             cat > /etc/systemd/system/remoteadb.service <<'UNIT'\n{unit}UNIT\n\
             systemctl daemon-reload\n\
             if systemctl is-active --quiet remoteadb.service 2>/dev/null; then\n\
             \x20   systemctl restart remoteadb.service\n\
             else\n\
             \x20   systemctl enable remoteadb.service\n\
             \x20   systemctl start remoteadb.service\n\
             fi\n"
        );

        let mut temp_sh = tempfile::Builder::new()
            .prefix("remoteadb_install-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(script.as_bytes())?;
        let sh_path = temp_sh.path().to_path_buf();
        temp_sh.keep()?;
        Ok(sh_path)
    }

    fn init_uninstall_script() -> anyhow::Result<PathBuf> {
        use std::io::Write as _;

        let mut temp_sh = tempfile::Builder::new()
            .prefix("remoteadb_uninstall-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(Self::UNINSTALL_SH_BYTES.as_bytes())?;
        let sh_path = temp_sh.path().to_path_buf();
        temp_sh.keep()?;
        Ok(sh_path)
    }

    fn user_unit_path() -> anyhow::Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("can't figure out config dir on this system")?
            .join("systemd")
            .join("user");
        Ok(dir.join("remoteadb.service"))
    }

    async fn install_user(service_params: ServiceParams) -> anyhow::Result<()> {
        let path = Self::user_unit_path()?;
        fs::create_dir_all(path.parent().expect("joined path")).await?;
        fs::write(&path, unit_file(&service_params, false)?).await?;
        println!("wrote {}", path.display());
        for args in [
            vec!["daemon-reload"],
            vec!["enable", "--now", "remoteadb.service"],
            vec!["restart", "remoteadb.service"],
        ] {
            let status = systemctl(true).args(&args).status()?;
            if !status.success() {
                anyhow::bail!("systemctl --user {} failed with {}", args.join(" "), status);
            }
        }
        Ok(())
    }

    async fn uninstall_user() -> anyhow::Result<()> {
        let path = Self::user_unit_path()?;
        let _ = systemctl(true)
            .args(["disable", "--now", "remoteadb.service"])
            .status();
        match fs::remove_file(&path).await {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
        }
        let _ = systemctl(true).arg("daemon-reload").status();
        Ok(())
    }
}
