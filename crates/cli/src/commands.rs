//! Subcommand implementations (info/ls/pull/push/mkdir/rm/mv).

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, bail};
use pereprava_core::ops::{self, TreeStats};
use pereprava_core::{DeviceHandle, Entry, Progress};
use tokio::sync::watch;

use crate::format::human_bytes;

/// Opens the first MTP device with a friendly error context.
pub async fn connect() -> anyhow::Result<DeviceHandle> {
    DeviceHandle::connect_first()
        .await
        .context("failed to open MTP device (run `pereprava doctor` for diagnostics)")
}

/// Runs `f` with a connected device and ALWAYS closes the session afterwards.
///
/// Skipping the close wedges Android's MTP server: the next connection times
/// out until the USB function is toggled.
pub async fn with_device<T, F, Fut>(f: F) -> anyhow::Result<T>
where
    F: FnOnce(DeviceHandle) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let dev = connect().await?;
    let out = f(dev.clone()).await;
    if let Err(e) = dev.close().await {
        tracing::debug!("close reported: {e}");
    }
    out
}

/// `info` — device and storage summary.
pub async fn info() -> anyhow::Result<()> {
    with_device(|dev| async move {
        let s = dev.info().await?;

        println!("Device:");
        if let Some(m) = &s.manufacturer {
            println!("  manufacturer : {m}");
        }
        if let Some(p) = &s.product {
            println!("  model        : {p}");
        }
        if let Some(sn) = &s.serial {
            println!("  serial       : {sn}");
        }
        if let Some(fw) = &s.firmware {
            println!("  firmware     : {fw}");
        }
        if let Some(sp) = &s.speed {
            println!("  usb speed    : {sp}");
        }

        println!("Storages:");
        for st in &s.storages {
            println!(
                "  {} — {} free / {}{} (id={})",
                st.description,
                human_bytes(st.free),
                human_bytes(st.capacity),
                if st.writable { "" } else { ", read-only" },
                st.id,
            );
        }
        Ok(())
    })
    .await
}

/// `ls` — list a directory (storages at `/`).
pub async fn ls(path: &str) -> anyhow::Result<()> {
    with_device(|dev| async move {
        let entries: Vec<Entry> = dev.list(path, false).await?;
        if entries.is_empty() {
            println!("(empty)");
            return Ok(());
        }
        let width = entries
            .iter()
            .map(|e| human_bytes(e.size).len())
            .max()
            .unwrap_or_default();
        for e in entries {
            let kind = if e.is_dir { 'd' } else { '-' };
            println!(
                "{kind} {:>width$} {}",
                human_bytes(e.size),
                e.name,
                width = width
            );
        }
        Ok(())
    })
    .await
}

/// `pull` — download a file or a directory tree.
pub async fn pull(remote: String, local: Option<String>) -> anyhow::Result<()> {
    with_device(move |dev| async move {
        let resolved = dev.resolve(&remote).await?;

        if resolved.entry.is_dir {
            let target = match local.clone() {
                Some(p) => PathBuf::from(p),
                None => {
                    let name = remote
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("device-dir");
                    std::env::current_dir()?.join(name)
                }
            };
            let t0 = Instant::now();
            let stats = ops::pull_tree(&dev, &remote, &target).await?;
            report_tree("pulled", &stats, t0);
            println!("  -> {}", target.display());
        } else {
            let file_path = match local.clone() {
                Some(p) => PathBuf::from(p),
                None => std::env::current_dir()?.join(resolved.entry.name.clone()),
            };
            let out = tokio::fs::File::create(&file_path)
                .await
                .with_context(|| format!("creating {}", file_path.display()))?;
            let (ptx, prx) = watch::channel(Progress {
                total: resolved.entry.size,
                done: 0,
            });
            let painter =
                crate::progress::spawn_progress(format!("pull {}", resolved.entry.name), prx);
            let t0 = Instant::now();
            let n = dev.download_into(&remote, Box::new(out), ptx).await?;
            drop(painter.await);
            println!(
                "{} -> {} in {:.2}s",
                human_bytes(n),
                file_path.display(),
                t0.elapsed().as_secs_f64()
            );
        }
        Ok(())
    })
    .await
}

/// `push` — upload a file or a directory tree.
///
/// Refuses to silently overwrite an existing remote file unless
/// `force` is set (Android MTP answers GeneralError to duplicate names,
/// so replacing means delete-then-upload).
pub async fn push(local: String, remote: Option<String>, force: bool) -> anyhow::Result<()> {
    with_device(move |dev| async move {
        let remote_dir = remote.unwrap_or_else(|| "/1".to_string());
        ensure_parent_exists(&dev, &remote_dir).await?;

        let lp = PathBuf::from(&local);
        let meta = tokio::fs::metadata(&lp)
            .await
            .with_context(|| format!("reading {}", lp.display()))?;

        if meta.is_dir() {
            let t0 = Instant::now();
            let stats = ops::push_tree(&dev, &lp, &remote_dir).await?;
            report_tree("pushed", &stats, t0);
        } else {
            let fname = lp
                .file_name()
                .and_then(|n| n.to_str())
                .context("local file name must be valid UTF-8")?
                .to_string();
            let target = ops::join_device(&remote_dir, &fname);
            match dev.resolve(&target).await {
                Ok(hit) if !hit.entry.is_dir && force => {
                    dev.remove(&target, false).await?;
                }
                Ok(_) => bail!("{target} already exists on device (use --force to replace)"),
                Err(_) => {} // fresh name
            }
            let file = tokio::fs::File::open(&lp).await?;
            let (ptx, prx) = watch::channel(Progress {
                total: meta.len(),
                done: 0,
            });
            let painter = crate::progress::spawn_progress(format!("push {fname}"), prx);
            let t0 = Instant::now();
            let entry = dev
                .upload_new(&remote_dir, &fname, meta.len(), Box::new(file), ptx)
                .await?;
            drop(painter.await);
            println!(
                "{} -> {remote_dir}/{} in {:.2}s",
                human_bytes(entry.size),
                entry.name,
                t0.elapsed().as_secs_f64()
            );
        }
        Ok(())
    })
    .await
}

async fn ensure_parent_exists(dev: &DeviceHandle, remote_dir: &str) -> anyhow::Result<()> {
    match dev.resolve(remote_dir).await {
        Ok(r) if r.entry.is_dir => Ok(()),
        Ok(_) => bail!("{remote_dir} is not a directory"),
        Err(_) => {
            bail!("remote directory {remote_dir} does not exist (create it with `pereprava mkdir`)")
        }
    }
}

/// `mkdir` — create a directory including missing parents.
pub async fn mkdir(path: String) -> anyhow::Result<()> {
    with_device(move |dev| async move {
        dev.mkdir_all(&path).await?;
        println!("created {path}");
        Ok(())
    })
    .await
}

/// `rm` — delete an object (directories need `-r`).
pub async fn rm(path: String, recursive: bool) -> anyhow::Result<()> {
    with_device(move |dev| async move {
        let n = dev.remove(&path, recursive).await?;
        println!("deleted {n} object(s): {path}");
        Ok(())
    })
    .await
}
/// `mv` — move into a directory, move+rename to a full path, or rename in
/// place when the destination is a bare file name.
pub async fn mv(from: String, to: String) -> anyhow::Result<()> {
    with_device(move |dev| async move {
        if let Ok(target) = dev.resolve(&to).await {
            if target.entry.is_dir {
                let moved = dev.move_into(&from, &to).await?;
                println!("moved -> {to}/{}", moved.name);
                return Ok(());
            }
            bail!("target {to} already exists");
        }

        // Bare destination name => rename within the source's parent.
        let (parent, leaf) = if !to.contains('/') {
            let idx = from
                .trim_end_matches('/')
                .rfind('/')
                .context("source must be an absolute device path like /storage/file")?;
            let p = from[..idx].to_string();
            if p.is_empty() {
                bail!("source parent cannot be the device root");
            }
            (p, to.clone())
        } else {
            split_device_parent_leaf(&to)
                .context("destination must look like <storage>/.../<name>")?
        };
        ensure_parent_exists(&dev, &parent).await?;

        let src = dev.resolve(&from).await?;
        let dst_dir = dev.resolve(&parent).await?;

        if dst_dir.handle == mtp_rs::ObjectHandle(src.entry.parent) {
            // Same directory: a pure rename (Android rejects no-op moves
            // with GeneralError, so don't even try move_object here).
            if src.entry.name != leaf {
                dev.rename(&from, &leaf).await?;
            }
            println!("renamed -> {to}");
            return Ok(());
        }

        let moved = dev.move_into(&from, &parent).await?;
        if moved.name != leaf {
            let current = ops::join_device(&parent, &moved.name);
            dev.rename(&current, &leaf).await?;
        }
        println!("moved+renamed -> {to}");
        Ok(())
    })
    .await
}

/// Splits `/a/b/c` into (`/a/b`, `c`). Storage-only paths have no leaf.
#[must_use]
pub fn split_device_parent_leaf(path: &str) -> Option<(String, String)> {
    let trimmed = path.trim();
    let idx = trimmed.rfind('/')?;
    if idx == 0 {
        return None;
    }
    let parent = trimmed[..idx].to_string();
    let leaf = trimmed[idx + 1..].to_string();
    if parent.is_empty() || leaf.is_empty() {
        return None;
    }
    Some((parent, leaf))
}

fn report_tree(verb: &str, stats: &TreeStats, t0: Instant) {
    println!(
        "{verb} {} file(s), {} dir(s), {} in {:.2}s",
        stats.files,
        stats.dirs,
        human_bytes(stats.bytes),
        t0.elapsed().as_secs_f64()
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn splits_parent_and_leaf() {
        let (p, l) =
            split_device_parent_leaf("/Internal storage/DCIM/a.jpg").expect("should split");
        assert_eq!(p, "/Internal storage/DCIM");
        assert_eq!(l, "a.jpg");
    }

    #[test]
    fn storage_root_has_no_leaf() {
        assert!(split_device_parent_leaf("/storage").is_none());
    }
}
