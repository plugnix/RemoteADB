//! Endpoint identity key storage. Derived from pigeons' ssh.rs
//! (https://github.com/n0-computer/pigeons), keeping its z32 encoding and
//! 0600 create-then-write handling, but storing the key in remoteadb's own
//! config directory rather than ~/.ssh.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use anyhow::Context;
use ed25519_dalek::SECRET_KEY_LENGTH;
use iroh::SecretKey;
use tokio::fs;

/// Load the z32-encoded secret key at `path`, generating and storing a new
/// one when it does not exist yet. The public half is the endpoint ID that
/// identifies this machine to its peers.
pub async fn load_or_create_key(path: &Path) -> anyhow::Result<SecretKey> {
    if path.exists() {
        tracing::debug!("loading key from {}", path.display());
        let encoded = fs::read(path)
            .await
            .with_context(|| format!("failed to read secret key from {}", path.display()))?;
        return decode_secret_key(&encoded)
            .with_context(|| format!("failed to load secret key from {}", path.display()));
    }

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    tracing::info!("generating new key at {}", path.display());
    let secret_key = SecretKey::generate();
    write_secret_key(path, &z32::encode(&secret_key.to_bytes())).await?;
    Ok(secret_key)
}

/// Write the secret key, readable only by its owner on unix.
///
/// The mode is set as the file is created rather than afterwards, so there is
/// no window in which the key sits on disk world-readable.
async fn write_secret_key(path: &Path, encoded: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt as _;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(encoded.as_bytes())
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        // Dropping a tokio File does not flush it; writes are dispatched to a
        // blocking pool and can still be in flight.
        file.sync_all()
            .await
            .with_context(|| format!("failed to flush {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, encoded)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Decode a z32-encoded secret key, checking the length so a truncated file
/// is reported rather than aborting the process.
fn decode_secret_key(encoded: &[u8]) -> anyhow::Result<SecretKey> {
    let decoded = z32::decode(encoded).context("secret key is not valid z32")?;
    let sk_bytes: [u8; SECRET_KEY_LENGTH] = decoded.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "secret key is {} bytes, expected {SECRET_KEY_LENGTH}",
            decoded.len()
        )
    })?;
    Ok(SecretKey::from_bytes(&sk_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_secret_key_round_trips() {
        let key = SecretKey::generate();
        let encoded = z32::encode(&key.to_bytes());
        let decoded = decode_secret_key(encoded.as_bytes()).unwrap();
        assert_eq!(decoded.to_bytes(), key.to_bytes());
    }

    #[test]
    fn decode_secret_key_rejects_truncated_key() {
        let key = SecretKey::generate();
        let encoded = z32::encode(&key.to_bytes());
        let truncated = &encoded.as_bytes()[..encoded.len() - 8];
        let err = decode_secret_key(truncated).unwrap_err().to_string();
        assert!(err.contains("expected 32"), "unexpected error: {err}");
    }

    #[test]
    fn decode_secret_key_rejects_garbage() {
        assert!(decode_secret_key(b"not z32 at all!!").is_err());
    }

    #[tokio::test]
    async fn generated_key_is_reloaded_not_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("key");
        let generated = load_or_create_key(&path).await.unwrap();
        let reloaded = load_or_create_key(&path).await.unwrap();
        assert_eq!(generated.to_bytes(), reloaded.to_bytes());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_key_is_owner_readable_only() {
        use std::{fs::metadata, os::unix::fs::PermissionsExt as _};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        load_or_create_key(&path).await.unwrap();
        let mode = metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got mode {:o}", mode & 0o777);
    }
}
