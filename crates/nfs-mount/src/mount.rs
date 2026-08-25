//! Mount automation: run the NFS server and attach it with macOS `mount_nfs`.
//!
//! Mounting requires administrator rights. We try the direct path first
//! (works when already root) and otherwise ask via `osascript`, which pops
//! the system GUI password dialog — once per mount.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// Attaches `127.0.0.1:/` at `mount_point` using NFSv3 over TCP loopback.
pub async fn mount(port: u16, mount_point: &Path) -> Result<()> {
    let mp = mount_point.display();
    // `soft` keeps a dead phone from wedging the volume permanently
    // (hard NFS + vanished USB = unkillable mount); retries stay modest.
    let script = format!(
        "mkdir -p '{mp}' && /sbin/mount_nfs -o soft,nolocks,vers=3,tcp,rsize=131072,wsize=131072,retry=1,retrans=2,timeo=50,port={port},mountport={port} 127.0.0.1:/ '{mp}'"
    );
    if sh("/bin/sh", &["-c", &script]).await.is_ok() {
        return Ok(());
    }
    // Not root: ask through the system dialog.
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    sh(
        "/usr/bin/osascript",
        &[
            "-e",
            &format!("do shell script \"{escaped}\" with administrator privileges"),
        ],
    )
    .await
    .context("mount_nfs needs administrator rights (GUI prompt was declined or failed)")?;
    Ok(())
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
