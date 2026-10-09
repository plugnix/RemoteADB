//! launchd service backend. Taken from pigeons
//! (https://github.com/n0-computer/pigeons) and modified: the daemon label and
//! script placeholders refer to remoteadb and its adb port, and the unused
//! `info` hook was dropped.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

use crate::{Service, ServiceParams};

#[derive(Debug, Clone)]
pub(crate) struct MacosService;

impl Service for MacosService {
    async fn install(service_params: ServiceParams) -> anyhow::Result<()> {
        if service_params.user {
            anyhow::bail!("--user is only supported on linux");
        }
        let path = Self::init_install_script(service_params)?;
        tracing::debug!("running install script: {}", path.display());
        run_sh(&path, "install")
    }

    async fn uninstall(_user: bool) -> anyhow::Result<()> {
        let path = Self::init_uninstall_script()?;
        tracing::debug!("running uninstall script: {}", path.display());
        run_sh(&path, "uninstall")
    }

    async fn restart(_user: bool) -> anyhow::Result<()> {
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", "system/com.plugnix.remoteadb"])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            anyhow::bail!("launchctl kickstart failed with exit code: {}", status);
        }
        Ok(())
    }
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

impl MacosService {
    const INSTALL_SH_BYTES: &str = include_str!("../../service/install_macos.sh");
    const UNINSTALL_SH_BYTES: &str = include_str!("../../service/uninstall_macos.sh");

    fn init_install_script(service_params: ServiceParams) -> anyhow::Result<PathBuf> {
        use std::io::Write as _;

        let mut relay_args = String::new();
        for url in &service_params.relay_url {
            relay_args.push_str(&format!(" --relay-url {url}"));
        }

        let mut temp_sh = tempfile::Builder::new()
            .prefix("remoteadb_install-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(
            Self::INSTALL_SH_BYTES
                .replace("[ADBPORT]", &service_params.adb_port.to_string())
                .replace("[RELAYARGS]", &relay_args)
                .replace(
                    "[BINARYPATH]",
                    service_params
                        .binary_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("binary path is not valid UTF-8"))?,
                )
                .as_bytes(),
        )?;
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
}
