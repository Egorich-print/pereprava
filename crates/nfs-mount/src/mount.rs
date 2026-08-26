//! Mount automation: run the NFS server and attach it with macOS `mount_nfs`.
//!
//! Mounting requires administrator rights. We try the direct path first
//! (works when already root) and otherwise ask via `osascript`, which pops
//! the system GUI password dialog — once per mount.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Attaches `127.0.0.1:/` near `mount_point` using NFSv3 over TCP loopback.
///
/// Returns the path actually used: when the requested point is occupied by
/// a stale/hung mount (kernel keeps ghost entries until reboot or explicit
/// root umount), we automatically fall back to `-2`, `-3`, ... so watchers
/// never wedge on leftovers they cannot remove without root.
pub async fn mount(port: u16, mount_point: &Path) -> Result<PathBuf> {
    let mut candidate = mount_point.to_path_buf();
    for attempt in 1..=9u32 {
        let mp = candidate.display();
        // `soft` keeps a dead phone from wedging the volume permanently
        // (hard NFS + vanished USB = unkillable mount); retries stay modest.
        let script = format!(
            "mkdir -p '{mp}' && /sbin/mount_nfs -o soft,nolocks,vers=3,tcp,rsize=131072,wsize=131072,retry=1,retrans=2,timeo=50,port={port},mountport={port} 127.0.0.1:/ '{mp}'"
        );
        if sh("/bin/sh", &["-c", &script]).await.is_ok() {
            return Ok(candidate);
        }
        // Not root: ask through the system dialog.
        let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
        if sh(
            "/usr/bin/osascript",
            &[
                "-e",
                &format!("do shell script \"{escaped}\" with administrator privileges"),
            ],
        )
        .await
        .is_ok()
        {
            return Ok(candidate);
        }
        candidate = mount_point.with_extension(format!("{}-{attempt}", ext_of(mount_point)));
        tracing::debug!(path=%candidate.display(), "mount fallback attempt");
    }
    bail!(
        "could not mount near {} (all fallback paths exhausted)",
        mount_point.display()
    )
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Detaches `mount_point`.
pub async fn unmount(mount_point: &Path) -> Result<()> {
    let mp = mount_point.display();
    let script = format!("umount '{mp}'");
    if sh("/bin/sh", &["-c", &script]).await.is_ok() {
        return Ok(());
    }
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    sh(
        "/usr/bin/osascript",
        &[
            "-e",
            &format!("do shell script \"{escaped}\" with administrator privileges"),
        ],
    )
    .await
    .with_context(|| format!("unmounting {mp}"))?;
    Ok(())
}

async fn sh(prog: &str, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new(prog)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawning {prog}"))?;
    if !out.status.success() {
        bail!(
            "{prog} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
