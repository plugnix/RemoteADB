//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    env, io,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

pub(crate) const DEFAULT_ADB_PORT: u16 = 5037;

/// A host saved on the client side under a friendly name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedHost {
    /// Ticket (or bare endpoint ID) of the remote host.
    pub ticket: String,
    /// Local port to expose that host's adb server on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// On-disk configuration, shared by the host and client roles.
///
/// Two copies can exist: a per-user one and a machine-wide one. The host
/// service running as root reads the machine-wide one; an unprivileged run
/// reads its own and falls back to the machine-wide one for anything unset.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Config {
    /// Private relay servers. Required: remoteadb never falls back to n0's
    /// public relays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relay_urls: Vec<String>,
    /// Port the local adb server listens on (host side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adb_port: Option<u16>,
    /// Path to the adb binary used to start the server (host side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adb_path: Option<String>,
    /// Endpoint IDs of clients allowed to use this host's adb server.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub allowed_clients: BTreeSet<String>,
    /// Remote hosts saved by name (client side).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, SavedHost>,
}

impl Config {
    /// Loads the effective config: the per-user file layered over the
    /// machine-wide one. Lists are merged, scalars prefer the user's value.
    pub async fn load() -> Result<Self> {
        let user = Self::load_from(&Paths::user()?.config).await?;
        let Ok(system) = Paths::system() else {
            return Ok(user);
        };
        let system = Self::load_from(&system.config).await?;
        Ok(user.layered_over(system))
    }

    /// Loads the config at the given paths only.
    pub async fn load_at(paths: &Paths) -> Result<Self> {
        Self::load_from(&paths.config).await
    }

    fn layered_over(self, base: Self) -> Self {
        let mut allowed = base.allowed_clients;
        allowed.extend(self.allowed_clients);
        let mut hosts = base.hosts;
        hosts.extend(self.hosts);
        Self {
            relay_urls: if self.relay_urls.is_empty() {
                base.relay_urls
            } else {
                self.relay_urls
            },
            adb_port: self.adb_port.or(base.adb_port),
            adb_path: self.adb_path.or(base.adb_path),
            allowed_clients: allowed,
            hosts,
        }
    }

    /// Read and parse the config at `path`. A config that has not been
    /// written yet yields the defaults.
    pub async fn load_from(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed reading config at {}", path.display()));
            }
        };
        toml::from_slice(&bytes)
            .with_context(|| format!("failed parsing config at {}", path.display()))
    }

    pub async fn store_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("config path has no parent")?;
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let tmp = dir.join(".config.remoteadb.tmp");
        fs::write(&tmp, toml::to_string(self)?)
            .await
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .await
            .with_context(|| format!("failed to rename temp file to {}", path.display()))?;
        Ok(())
    }

    pub fn adb_port(&self) -> u16 {
        self.adb_port.unwrap_or(DEFAULT_ADB_PORT)
    }
}

/// Where this process keeps its config, key, and published ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub dir: PathBuf,
    pub config: PathBuf,
    pub key: PathBuf,
    pub ticket: PathBuf,
}

impl Paths {
    fn in_dir(dir: PathBuf) -> Self {
        Self {
            config: dir.join("config.toml"),
            key: dir.join("key"),
            ticket: dir.join("ticket"),
            dir,
        }
    }

    /// Per-user paths.
    pub fn user() -> Result<Self> {
        let dir = dirs::config_dir()
            .context("can't figure out config dir on this system")?
            .join("remoteadb");
        Ok(Self::in_dir(dir))
    }

    /// Machine-wide paths, used by the host when it runs as a system service.
    pub fn system() -> Result<Self> {
        let dir = match env::consts::OS {
            "linux" | "macos" => PathBuf::from("/etc/remoteadb"),
            "windows" => PathBuf::from("C:\\ProgramData\\remoteadb"),
            other => anyhow::bail!("no machine-wide config location on {other}"),
        };
        Ok(Self::in_dir(dir))
    }

    /// The paths this process should use: machine-wide when elevated,
    /// otherwise the user's own.
    pub fn resolve(system: bool) -> Result<Self> {
        if system { Self::system() } else { Self::user() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config() {
        let config = toml::from_str::<Config>("").unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn layering_merges_lists_and_prefers_user_scalars() {
        let user = Config {
            relay_urls: vec![],
            adb_port: Some(5038),
            adb_path: None,
            allowed_clients: ["a".to_string()].into_iter().collect(),
            hosts: BTreeMap::new(),
        };
        let system = Config {
            relay_urls: vec!["https://relay.example".to_string()],
            adb_port: Some(5037),
            adb_path: Some("/usr/bin/adb".to_string()),
            allowed_clients: ["b".to_string()].into_iter().collect(),
            hosts: BTreeMap::new(),
        };
        let merged = user.layered_over(system);
        assert_eq!(merged.relay_urls, vec!["https://relay.example"]);
        assert_eq!(merged.adb_port, Some(5038));
        assert_eq!(merged.adb_path.as_deref(), Some("/usr/bin/adb"));
        assert_eq!(merged.allowed_clients.len(), 2);
    }

    #[tokio::test]
    async fn config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let mut config = Config::default();
        config.relay_urls.push("https://relay.example".to_string());
        config.allowed_clients.insert("abc".to_string());
        config.hosts.insert(
            "buildbox".to_string(),
            SavedHost {
                ticket: "endpoint...".to_string(),
                port: Some(5038),
            },
        );
        config.store_to(&path).await.unwrap();
        let loaded = Config::load_from(&path).await.unwrap();
        assert_eq!(loaded, config);
    }

    #[tokio::test]
    async fn load_from_missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("config.toml"))
            .await
            .unwrap();
        assert_eq!(config, Config::default());
    }
}
