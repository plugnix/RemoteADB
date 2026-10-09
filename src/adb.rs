//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{process::Stdio, time::Duration};

use tokio::{net::TcpStream, process::Command, time::sleep};
use tokio_util::sync::CancellationToken;

/// Whether something accepts TCP connections on the adb server port.
pub async fn adb_listening(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).await.is_ok()
}

/// Keep an adb server alive on `port` until `cancel` fires.
///
/// If a server is already listening (for example one started by Android
/// Studio) it is left alone and only probed. Otherwise `adb nodaemon server`
/// is started as a child process and restarted whenever it exits, which also
/// covers a remote client whose adb version mismatch made it kill the server.
pub async fn supervise_adb(adb_path: String, port: u16, cancel: CancellationToken) {
    let probe_interval = Duration::from_secs(5);
    let restart_delay = Duration::from_secs(2);
    loop {
        if cancel.is_cancelled() {
            return;
        }
        if adb_listening(port).await {
            tokio::select! {
                _ = sleep(probe_interval) => {}
                _ = cancel.cancelled() => return,
            }
            continue;
        }

        tracing::info!("starting adb server on 127.0.0.1:{port} with {adb_path}");
        let child = Command::new(&adb_path)
            .args(["-L", &format!("tcp:127.0.0.1:{port}"), "nodaemon", "server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                tracing::error!("failed to start {adb_path}: {err}");
                tokio::select! {
                    _ = sleep(probe_interval) => {}
                    _ = cancel.cancelled() => return,
                }
                continue;
            }
        };

        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(status) => tracing::warn!("adb server exited with {status}, restarting"),
                    Err(err) => tracing::error!("waiting on adb server failed: {err}"),
                }
            }
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                return;
            }
        }
        tokio::select! {
            _ = sleep(restart_delay) => {}
            _ = cancel.cancelled() => return,
        }
    }
}
