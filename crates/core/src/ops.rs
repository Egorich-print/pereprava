//! Recursive tree operations (pull/push) built on top of the device actor.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::sync::watch;

use crate::actor::DeviceHandle;
use crate::error::Result;
use crate::model::Progress;

/// Counters produced by a recursive transfer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeStats {
    /// Files transferred.
    pub files: u32,
    /// Directories created / walked.
    pub dirs: u32,
    /// Bytes transferred.
    pub bytes: u64,
}

/// Pulls `remote_dir` (recursively) into `local_root`.
///
/// `local_root` is created when missing.
///
/// # Errors
/// Propagates actor/device errors; aborts on first failure.
pub fn pull_tree<'a>(
    dev: &'a DeviceHandle,
    remote_dir: &'a str,
    local_root: &'a Path,
) -> Pin<Box<dyn Future<Output = Result<TreeStats>> + Send + 'a>> {
    Box::pin(pull_tree_inner(dev, remote_dir, local_root))
}

async fn pull_tree_inner(
    dev: &DeviceHandle,
    remote_dir: &str,
    local_root: &Path,
) -> Result<TreeStats> {
    tokio::fs::create_dir_all(local_root).await?;
    let entries = dev.list(remote_dir, false).await?;
    let mut stats = TreeStats {
        dirs: 1,
        ..TreeStats::default()
    };

    for e in entries {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let child_local = local_root.join(&e.name);
        if e.is_dir {
            let child_remote = join_device(remote_dir, &e.name);
            let sub = pull_tree(dev, &child_remote, &child_local).await?;
            stats.files += sub.files;
            stats.dirs += sub.dirs;
            stats.bytes += sub.bytes;
        } else {
            let file = tokio::fs::File::create(&child_local).await?;
            let (ptx, _prx) = watch::channel(Progress {
                total: e.size,
                done: 0,
            });
            let n = dev
                .download_into(&join_device(remote_dir, &e.name), Box::new(file), ptx)
                .await?;
            stats.files += 1;
            stats.bytes += n;
        }
    }
    Ok(stats)
}

/// Pushes the local directory `local_dir` into remote directory
/// `remote_parent` (creating `<remote_parent>/<local_dir_name>`).
///
/// # Errors
/// Propagates actor/device errors; aborts on first failure.
pub async fn push_tree(
    dev: &DeviceHandle,
    local_dir: &Path,
    remote_parent: &str,
) -> Result<TreeStats> {
    let name = local_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            crate::error::Error::InvalidArgument(format!(
                "cannot derive a directory name from {}",
                local_dir.display()
            ))
        })?;
    let remote_here = join_device(remote_parent, name);
    dev.mkdir_all(&remote_here).await?;

    let mut stats = TreeStats {
        dirs: 1,
        ..TreeStats::default()
    };
    push_contents(dev, local_dir, &remote_here, &mut stats).await?;
    Ok(stats)
}

fn push_contents<'a>(
    dev: &'a DeviceHandle,
    local_dir: &'a Path,
    remote_here: &'a str,
    stats: &'a mut TreeStats,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(push_contents_inner(dev, local_dir, remote_here, stats))
}

async fn push_contents_inner(
    dev: &DeviceHandle,
    local_dir: &Path,
    remote_here: &str,
    stats: &mut TreeStats,
) -> Result<()> {
    let mut rd = tokio::fs::read_dir(local_dir).await?;
    while let Some(item) = rd.next_entry().await? {
        let path = item.path();
        let fname = match item.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue, // non-UTF8 names are out of scope for MTP
        };
        if item.file_type().await?.is_dir() {
            let sub_remote = join_device(remote_here, &fname);
            dev.mkdir_all(&sub_remote).await?;
            stats.dirs += 1;
            push_contents(dev, &path, &sub_remote, stats).await?;
        } else {
            let meta = item.metadata().await?;
            let file = tokio::fs::File::open(&path).await?;
            let (ptx, _prx) = watch::channel(Progress {
                total: meta.len(),
                done: 0,
            });
            dev.upload_new(remote_here, &fname, meta.len(), Box::new(file), ptx)
                .await?;
            stats.files += 1;
            stats.bytes += meta.len();
        }
    }
    Ok(())
}

/// Joins two device-path components with `/`, tolerating empty parts.
#[must_use]
pub fn join_device(base: &str, leaf: &str) -> String {
    let mut s = base.trim_end_matches('/').to_string();
    s.push('/');
    s.push_str(leaf.trim_start_matches('/'));
    s
}

/// Convenience for callers that want a throwaway progress channel.
#[must_use]
pub fn silent_progress() -> (watch::Sender<Progress>, watch::Receiver<Progress>) {
    watch::channel(Progress { total: 0, done: 0 })
}

/// Local path helper used by CLI to normalize destinations.
#[must_use]
pub fn ensure_unique_suffix(p: PathBuf) -> PathBuf {
    // v0.1 keeps it simple: caller decides overwrite policy.
    p
}
