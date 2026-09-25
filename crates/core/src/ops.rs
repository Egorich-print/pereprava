//! Recursive tree operations (pull/push) built on top of the device actor.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::sync::watch;

use crate::actor::DeviceHandle;
use crate::error::Result;
use crate::model::Progress;

/// True when `name` is a single, ordinary path component that is safe to join
/// onto a local directory.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// Guard against pathological (or hostile) device trees: every level of
/// nesting boxes a future, so unbounded depth would grow the heap without
/// limit before any I/O happened.
pub const MAX_TREE_DEPTH: usize = 64;

/// Rejects a walk that is already deeper than [`MAX_TREE_DEPTH`].
fn check_depth(depth: usize, at: &str) -> Result<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(crate::error::Error::InvalidArgument(format!(
            "directory nesting exceeds {MAX_TREE_DEPTH} levels at {at}"
        )));
    }
    Ok(())
}

/// Counters produced by a recursive transfer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeStats {
    /// Files transferred.
    pub files: u32,
    /// Directories created / walked.
    pub dirs: u32,
    /// Bytes transferred.
    pub bytes: u64,
    /// Entries that were deliberately **not** transferred: names that are not
    /// safe components, symlinks pointing outside the tree, special files, or
    /// names MTP cannot represent. A non-zero value means the transfer is
    /// incomplete and must be surfaced to the user rather than reported as a
    /// clean success.
    pub skipped: u32,
}

/// A unique temporary sibling of `path`, so a partially written file never
/// clobbers the previous version of `path` and a rename is atomic.
fn temp_sibling(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_string());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(".{name}.pereprava-{}-{n}.tmp", std::process::id()))
}

/// Pulls `remote_dir` (recursively) into `local_root`.
///
/// `local_root` is created when missing. Files are streamed to a temporary
/// sibling and renamed into place only after a complete transfer, so a failed
/// download never truncates an existing file and a pre-existing symlink at the
/// destination is replaced rather than followed.
///
/// # Errors
/// Propagates actor/device errors; aborts on first failure. Entries that are
/// skipped (unsafe names, symlinks, unrepresentable names) are counted in
/// [`TreeStats::skipped`] rather than silently dropped.
pub fn pull_tree<'a>(
    dev: &'a DeviceHandle,
    remote_dir: &'a str,
    local_root: &'a Path,
) -> Pin<Box<dyn Future<Output = Result<TreeStats>> + Send + 'a>> {
    Box::pin(pull_tree_inner(dev, remote_dir, local_root, 0))
}

/// Boxed recursion step: an `async fn` cannot recurse directly.
fn pull_tree_step<'a>(
    dev: &'a DeviceHandle,
    remote_dir: &'a str,
    local_root: &'a Path,
    depth: usize,
) -> Pin<Box<dyn Future<Output = Result<TreeStats>> + Send + 'a>> {
    Box::pin(pull_tree_inner(dev, remote_dir, local_root, depth))
}

async fn pull_tree_inner(
    dev: &DeviceHandle,
    remote_dir: &str,
    local_root: &Path,
    depth: usize,
) -> Result<TreeStats> {
    check_depth(depth, remote_dir)?;
    tokio::fs::create_dir_all(local_root).await?;
    let entries = dev.list(remote_dir, false).await?;
    let mut stats = TreeStats {
        dirs: 1,
        ..TreeStats::default()
    };

    for e in entries {
        // Device-reported names are untrusted: reject anything that is not a
        // single normal path component so a hostile/buggy device cannot make
        // us write outside `local_root`.
        if !is_safe_component(&e.name) {
            tracing::warn!("skipping unsafe device entry name {:?}", e.name);
            stats.skipped += 1;
            continue;
        }
        let child_local = local_root.join(&e.name);
        if e.is_dir {
            let child_remote = join_device(remote_dir, &e.name);
            let sub = pull_tree_step(dev, &child_remote, &child_local, depth + 1).await?;
            stats.files += sub.files;
            stats.dirs += sub.dirs;
            stats.bytes += sub.bytes;
            stats.skipped += sub.skipped;
        } else {
            // Stage into a temp sibling, then rename: an existing file is only
            // replaced once the full content has arrived, and `rename` swaps
            // the destination path itself (so a symlink there is replaced, not
            // followed).
            let tmp = temp_sibling(&child_local);
            let file = match tokio::fs::File::create(&tmp).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("cannot stage {}: {e}", child_local.display());
                    stats.skipped += 1;
                    continue;
                }
            };
            let (ptx, _prx) = watch::channel(Progress {
                total: e.size,
                done: 0,
            });
            match dev
                .download_into(&join_device(remote_dir, &e.name), Box::new(file), ptx)
                .await
            {
                Ok(n) => {
                    drop(_prx);
                    tokio::fs::rename(&tmp, &child_local).await?;
                    stats.files += 1;
                    stats.bytes += n;
                }
                Err(err) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(err);
                }
            }
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
    push_contents(dev, local_dir, &remote_here, 0, &mut stats).await?;
    Ok(stats)
}

/// Boxed recursion step for [`push_contents_inner`].
fn push_contents<'a>(
    dev: &'a DeviceHandle,
    local_dir: &'a Path,
    remote_here: &'a str,
    depth: usize,
    stats: &'a mut TreeStats,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(push_contents_inner(
        dev,
        local_dir,
        remote_here,
        depth,
        stats,
    ))
}

async fn push_contents_inner(
    dev: &DeviceHandle,
    local_dir: &Path,
    remote_here: &str,
    depth: usize,
    stats: &mut TreeStats,
) -> Result<()> {
    check_depth(depth, &local_dir.display().to_string())?;
    let mut rd = tokio::fs::read_dir(local_dir).await?;
    while let Some(item) = rd.next_entry().await? {
        let path = item.path();
        // `file_type()` does not follow symlinks; `metadata()` does. Using
        // `metadata()` meant a symlink inside the tree uploaded whatever it
        // pointed at — a file outside the tree, or a device node.
        let ftype = item.file_type().await?;
        if ftype.is_symlink() {
            tracing::warn!("skipping symlink {}", path.display());
            stats.skipped += 1;
            continue;
        }
        let fname = match item.file_name().to_str() {
            Some(s) => s.to_string(),
            None => {
                // MTP names are UTF-8; this entry cannot be represented.
                tracing::warn!("skipping non-UTF-8 name {}", path.display());
                stats.skipped += 1;
                continue;
            }
        };
        if ftype.is_dir() {
            let sub_remote = join_device(remote_here, &fname);
            dev.mkdir_all(&sub_remote).await?;
            stats.dirs += 1;
            push_contents(dev, &path, &sub_remote, depth + 1, stats).await?;
        } else if ftype.is_file() {
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
        } else {
            // Sockets, FIFOs and device nodes have no MTP representation and
            // would block or misbehave if opened.
            tracing::warn!("skipping non-regular file {}", path.display());
            stats.skipped += 1;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn safe_components_reject_traversal_and_separators() {
        assert!(is_safe_component("photo.jpg"));
        assert!(is_safe_component("Внутренний накопитель"));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component("a\\b"));
        assert!(!is_safe_component("a\0b"));
    }

    #[test]
    fn temp_sibling_is_unique_and_stays_beside_the_target() {
        let target = Path::new("/tmp/out/photo.jpg");
        let a = temp_sibling(target);
        let b = temp_sibling(target);
        assert_ne!(a, b, "each staging file needs a unique name");
        assert_eq!(
            a.parent(),
            target.parent(),
            "must be on the same filesystem"
        );
        assert!(a.to_string_lossy().ends_with(".tmp"));
        // The visible name must stay out of the way of the final result.
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".photo.jpg.")
        );
    }

    #[test]
    fn stats_start_clean_and_count_skips() {
        let mut s = TreeStats::default();
        assert_eq!(s.skipped, 0);
        s.skipped += 1;
        assert_eq!(s.skipped, 1);
    }

    #[test]
    fn depth_guard_allows_up_to_the_limit_and_refuses_beyond() {
        assert!(check_depth(0, "/1").is_ok());
        assert!(check_depth(MAX_TREE_DEPTH, "/1").is_ok());
        let err = check_depth(MAX_TREE_DEPTH + 1, "/1/deep").expect_err("must refuse");
        assert!(err.to_string().contains(&MAX_TREE_DEPTH.to_string()));
    }
}
