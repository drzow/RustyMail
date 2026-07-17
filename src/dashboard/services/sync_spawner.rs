// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Shared helper for locating and spawning the `rustymail-sync` binary.
//!
//! The sync engine deliberately runs as a separate process so the OS reclaims
//! its memory when it exits (see `src/bin/sync.rs`). Three call sites spawn it —
//! the REST `trigger_email_sync` handler, the MCP `sync_emails` tool, and the
//! background spawners in `main.rs`. This module is the single definition of the
//! locate + spawn + reap logic they all share.

use log::{error, info};

/// Locate the `rustymail-sync` binary across build/deploy layouts.
/// Order (superset of what any caller used before): release, debug, cwd, PATH.
pub fn locate_sync_binary() -> &'static str {
    if std::path::Path::new("./target/release/rustymail-sync").exists() {
        "./target/release/rustymail-sync"
    } else if std::path::Path::new("./target/debug/rustymail-sync").exists() {
        "./target/debug/rustymail-sync"
    } else if std::path::Path::new("./rustymail-sync").exists() {
        "./rustymail-sync"
    } else {
        "rustymail-sync"
    }
}

/// Outcome of spawning the sync binary.
pub enum SpawnOutcome {
    /// The process is running in the background (reaper detached).
    Started { pid: u32 },
    /// Another sync holds the lock file (binary exited with code 2).
    AlreadyRunning,
    /// The process finished immediately and successfully (e.g. empty sync).
    Completed,
}

/// Spawn `rustymail-sync` with `args`, wait 100ms, and classify via `try_wait`:
/// exit code 2 => `AlreadyRunning`; clean immediate exit => `Completed`;
/// still running (or status-check error) => `Started` with a detached reaper so
/// the child is never left as a zombie; a non-2 error exit => `Err`.
pub fn spawn_sync(args: &[String]) -> std::io::Result<SpawnOutcome> {
    let binary = locate_sync_binary();
    let mut cmd = std::process::Command::new(binary);
    cmd.args(args);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            error!("Failed to spawn sync process '{}': {}", binary, e);
            return Err(e);
        }
    };

    let pid = child.id();
    info!("Spawned sync process (pid: {}) args={:?}", pid, args);

    // Wait briefly to catch an immediate "already running" exit (code 2).
    std::thread::sleep(std::time::Duration::from_millis(100));

    match child.try_wait() {
        Ok(Some(status)) => {
            if status.code() == Some(2) {
                info!("Sync process reports another sync is already in progress");
                Ok(SpawnOutcome::AlreadyRunning)
            } else if status.success() {
                Ok(SpawnOutcome::Completed)
            } else {
                error!("Sync process exited with error code: {:?}", status.code());
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Sync process failed with exit code: {:?}", status.code()),
                ))
            }
        }
        Ok(None) => {
            // Still running (normal case). Detach a reaper so nothing lingers as a
            // zombie — std::process::Child does not reap on drop.
            tokio::task::spawn_blocking(move || {
                let _ = child.wait();
            });
            Ok(SpawnOutcome::Started { pid })
        }
        Err(e) => {
            error!("Failed to check sync process status: {}", e);
            // Assume it is running; still reap it on exit to avoid a zombie.
            tokio::task::spawn_blocking(move || {
                let _ = child.wait();
            });
            Ok(SpawnOutcome::Started { pid })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_sync_binary_returns_path() {
        let path = locate_sync_binary();
        let known = [
            "./target/release/rustymail-sync",
            "./target/debug/rustymail-sync",
            "./rustymail-sync",
            "rustymail-sync",
        ];
        assert!(known.contains(&path), "unexpected sync binary path: {}", path);
    }
}
