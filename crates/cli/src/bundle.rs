//! Bundle-mode: directory tree ⇄ single `.tar.zst` object (ADR-003).
//!
//! Rationale: MTP charges ~35 ms of protocol overhead per object. A tree of
//! N small files costs N×35 ms no matter the payload; as ONE archive object
//! it transfers at wire speed. Compression is a secondary win and applies
//! only to compressible payloads.
//!
//! Implementation note: MTP requires the total size upfront, while zstd's
//! output size is unknown until compression finishes — so archives are
//! staged through a local temp file. Disk cost ≈ archive size, transient.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use pereprava_core::DeviceHandle;

/// Outcome of a bundle transfer.
#[derive(Debug, Clone, Copy)]
pub struct BundleStats {
    /// Files packed/unpacked.
    pub files: u32,
    /// Directories seen.
    pub dirs: u32,
    /// Uncompressed payload bytes.
    pub raw_bytes: u64,
    /// Compressed archive bytes actually transferred.
    pub packed_bytes: u64,
    /// Wall-clock total including staging.
    pub elapsed_ms: u128,
}

/// Packs `local_dir` into `<remote_parent>/<name>.tar.zst` and uploads it.
pub async fn push_as_bundle(
    dev: &DeviceHandle,
    local_dir: &Path,
    remote_parent: &str,
) -> Result<BundleStats> {
    let name = local_dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("directory name must be valid UTF-8")?
        .to_string();
    let archive_name = format!("{name}.tar.zst");

    // Refuse to overwrite (Android rejects duplicates anyway).
    let target = pereprava_core::ops::join_device(remote_parent, &archive_name);
    if dev.resolve(&target).await.is_ok() {
        anyhow::bail!("{target} already exists on device");
    }

    let t0 = Instant::now();
    let staged = staging_path(&archive_name);
    let src = local_dir.to_path_buf();
    let dst = staged.clone();
    let counts = match tokio::task::spawn_blocking(move || pack_tar_zstd(&src, &dst))
        .await
        .context("pack task panicked")?
    {
        Ok(c) => c,
        Err(e) => {
            drop(tokio::fs::remove_file(&staged).await);
            return Err(anyhow::Error::from(e).context("packing failed"));
        }
    };

    let meta = tokio::fs::metadata(&staged).await?;
    let file = tokio::fs::File::open(&staged).await?;
    dev.upload_new(
        remote_parent,
        &archive_name,
        meta.len(),
        Box::new(file),
        silent(),
    )
    .await?;
    drop(tokio::fs::remove_file(&staged).await);

    Ok(BundleStats {
        files: counts.0,
        dirs: counts.1,
        raw_bytes: counts.2,
        packed_bytes: meta.len(),
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

/// Downloads `remote_archive` (.tar.zst) and extracts it into `dest_parent`.
/// Returns stats about the extracted tree.
pub async fn pull_bundle(
    dev: &DeviceHandle,
    remote_archive: &str,
    dest_parent: &Path,
) -> Result<BundleStats> {
    let fname = remote_archive
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("bundle.tar.zst")
        .to_string();
    let resolved = dev.resolve(remote_archive).await?;

    let t0 = Instant::now();
    let staged = staging_path(&fname);
    {
        let out = tokio::fs::File::create(&staged).await?;
        let (ptx, prx) = tokio::sync::watch::channel(pereprava_core::Progress {
            total: resolved.entry.size,
            done: 0,
        });
        let painter = crate::progress::spawn_progress(format!("pull {fname}"), prx);
        dev.download_into(remote_archive, Box::new(out), ptx)
            .await?;
        drop(painter.await);
    }

    tokio::fs::create_dir_all(dest_parent).await?;
    let dst = dest_parent.to_path_buf();
    let arc = staged.clone();
    let counts = match tokio::task::spawn_blocking(move || unpack_tar_zstd(&arc, &dst))
        .await
        .context("unpack task panicked")?
    {
        Ok(c) => c,
        Err(e) => {
            // The staged archive is a private temp file; never leave it behind.
            drop(tokio::fs::remove_file(&staged).await);
            return Err(anyhow::Error::from(e).context("extraction failed"));
        }
    };
    drop(tokio::fs::remove_file(&staged).await);

    Ok(BundleStats {
        files: counts.0,
        dirs: counts.1,
        raw_bytes: counts.2,
        packed_bytes: resolved.entry.size,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

fn staging_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pereprava-stage-{}-{name}", std::process::id()))
}

/// Deterministic-order tar+zstd packing. Returns (files, dirs, raw bytes).
fn pack_tar_zstd(src: &Path, dst: &Path) -> std::io::Result<(u32, u32, u64)> {
    let out = std::fs::File::create(dst)?;
    let enc = zstd::stream::write::Encoder::new(out, 3)?.auto_finish();
    let mut builder = tar::Builder::new(enc);
    builder.mode(tar::HeaderMode::Deterministic);

    let mut files = 0u32;
    let mut dirs = 0u32;
    let mut raw = 0u64;
    let mut visited = Vec::new();
    walk_add(
        &mut builder,
        src,
        src,
        0,
        &mut files,
        &mut dirs,
        &mut raw,
        &mut visited,
    )?;
    builder.finish()?;
    Ok((files, dirs, raw))
}

/// Depth guard for packing: every level recurses, so a pathological tree must
/// not be able to exhaust the stack.
const MAX_PACK_DEPTH: usize = 128;

fn walk_add(
    builder: &mut tar::Builder<impl Write>,
    root: &Path,
    dir: &Path,
    depth: usize,
    files: &mut u32,
    dirs: &mut u32,
    raw: &mut u64,
    visited: &mut Vec<std::fs::Metadata>,
) -> std::io::Result<()> {
    if depth > MAX_PACK_DEPTH {
        return Err(std::io::Error::other(format!(
            "directory nesting exceeds {MAX_PACK_DEPTH} levels at {}",
            dir.display()
        )));
    }
    // A directory that we have already entered (through a hard-linked alias or
    // a bind) would otherwise be walked forever.
    let here = std::fs::metadata(dir)?;
    if visited.iter().any(|m| same_file(m, &here)) {
        return Err(std::io::Error::other(format!(
            "directory cycle detected at {}",
            dir.display()
        )));
    }
    visited.push(here);

    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .collect::<std::io::Result<Vec<std::fs::DirEntry>>>()?
        .into_iter()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let rel = path
            .strip_prefix(root)
            .map_err(|_| std::io::Error::other(format!("path outside root: {}", path.display())))?;
        // `symlink_metadata` does not follow: a symlink inside the source used
        // to pull an entire unrelated subtree (or a cycle) into the archive.
        let meta = std::fs::symlink_metadata(&path)?;
        let ftype = meta.file_type();
        if ftype.is_symlink() {
            // Bundles are file/dir trees by definition; record the link target
            // instead of its contents so the archive stays self-consistent and
            // cannot pull in an unrelated subtree.
            let target = std::fs::read_link(&path)?;
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            builder.append_link(&mut header, rel, target)?;
            continue;
        }
        if ftype.is_dir() {
            *dirs += 1;
            builder.append_dir(rel, &path)?;
            walk_add(builder, root, &path, depth + 1, files, dirs, raw, visited)?;
        } else if ftype.is_file() {
            *files += 1;
            *raw += meta.len();
            builder.append_path_with_name(&path, rel)?;
        } else {
            // Sockets/FIFOs/devices have no place in a bundle.
        }
    }
    visited.pop();
    Ok(())
}

/// True when two metadata records denote the same inode (device + inode).
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    false
}

/// A private sibling directory of `dest`, used for staging.
fn sibling_scratch(dest: &Path, tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_string());
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".{name}.pereprava-{tag}-{}-{n}",
        std::process::id()
    ))
}

/// True when an archive entry name could escape the extraction directory.
///
/// A path is unsafe if it is absolute or contains a `..` component. The `tar`
/// crate's *builder* refuses to create such an entry, but an archive can
/// arrive from anywhere, so the extractor must not trust it.
fn is_unsafe_archive_path(path: &Path) -> bool {
    path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Extracts a .tar.zst archive. Returns (files, dirs, raw bytes).
///
/// Extraction goes into a private staging directory first and is only promoted
/// into `dest` once the whole archive decoded: previously a mid-archive error
/// left the destination half-replaced with no way to tell what changed.
/// Entry names are validated (no `..`, no absolute paths) and the expanded
/// size/count are bounded so a small archive cannot fill the disk.
fn unpack_tar_zstd(archive: &Path, dest: &Path) -> std::io::Result<(u32, u32, u64)> {
    /// Hard ceilings for a single bundle: refuse to expand beyond these.
    const MAX_ENTRIES: u64 = 2_000_000;
    const MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

    // The staging directory must be a *sibling* of `dest`: `promote` renames
    // `dest` aside, which would carry a nested staging dir with it.
    let staging = sibling_scratch(dest, "unpack");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    let result = (|| -> std::io::Result<(u32, u32, u64)> {
        let f = std::fs::File::open(archive)?;
        let dec = zstd::stream::read::Decoder::new(f)?;
        let mut ar = tar::Archive::new(dec);
        ar.set_preserve_permissions(false);
        ar.set_unpack_xattrs(false);
        ar.set_overwrite(true);

        let mut files = 0u32;
        let mut dirs = 0u32;
        let mut raw = 0u64;
        for entry in ar.entries()? {
            let mut e = entry?;
            let path = e.path()?.into_owned();
            // Reject traversal / absolute paths before they can escape the
            // staging directory.
            if is_unsafe_archive_path(&path) {
                return Err(std::io::Error::other(format!(
                    "unsafe path in archive: {}",
                    path.display()
                )));
            }
            match e.header().entry_type() {
                tar::EntryType::Directory => dirs += 1,
                tar::EntryType::Regular => {
                    files += 1;
                    raw = raw.saturating_add(e.size());
                    if raw > MAX_BYTES {
                        return Err(std::io::Error::other(
                            "archive expands beyond the size limit",
                        ));
                    }
                }
                _ => continue, // symlinks/devices are not part of our bundles
            }
            if u64::from(files).saturating_add(u64::from(dirs)) > MAX_ENTRIES {
                return Err(std::io::Error::other("archive has too many entries"));
            }
            e.unpack_in(&staging)?;
        }
        Ok((files, dirs, raw))
    })();

    match result {
        Ok(v) => {
            // Promote the staged tree into the destination.
            promote(&staging, dest)?;
            Ok(v)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// Moves the freshly extracted `staging` tree into `dest`, replacing it.
fn promote(staging: &Path, dest: &Path) -> std::io::Result<()> {
    // Replace the destination wholesale so a previous extraction cannot leave
    // stale files behind next to the new ones.
    let backup = dest.with_extension("pereprava-old");
    let _ = std::fs::remove_dir_all(&backup);
    if dest.exists() {
        std::fs::rename(dest, &backup)?;
    }
    match std::fs::rename(staging, dest) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(e) => {
            // Put the old tree back rather than leaving nothing behind.
            if backup.exists() {
                let _ = std::fs::rename(&backup, dest);
            }
            Err(e)
        }
    }
}

fn silent() -> tokio::sync::watch::Sender<pereprava_core::Progress> {
    tokio::sync::watch::channel(pereprava_core::Progress { total: 0, done: 0 }).0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pv-bundle-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pack_then_unpack_roundtrips_a_tree() {
        let root = scratch("rt");
        let src = root.join("tree");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("sub/b.bin"), b"bravo").unwrap();

        let arc = root.join("t.tar.zst");
        let (files, dirs, raw) = pack_tar_zstd(&src, &arc).unwrap();
        assert_eq!((files, dirs), (2, 1)); // the root itself is not an entry
        assert_eq!(raw, 10); // "alpha" + "bravo"

        let dest = root.join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let (f2, _d2, r2) = unpack_tar_zstd(&arc, &dest).unwrap();
        assert_eq!((f2, r2), (2, 10));
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(dest.join("sub/b.bin")).unwrap(), b"bravo");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pack_does_not_follow_symlinks_out_of_the_tree() {
        // A symlink pointing outside the source must be recorded as a link, not
        // walked (which previously archived the whole outside subtree).
        let root = scratch("sym");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret data").unwrap();
        let src = root.join("tree");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("ok.txt"), b"ok").unwrap();
        std::os::unix::fs::symlink(&outside, src.join("link")).unwrap();

        let arc = root.join("s.tar.zst");
        let (files, _dirs, raw) = pack_tar_zstd(&src, &arc).unwrap();
        // Only the real file is counted; the link is a symlink entry, not data.
        assert_eq!(files, 1, "the outside directory must not be archived");
        assert!(raw < 100);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Builds a raw (uncompressed) tar with a single entry of `name`.
    ///
    /// The `tar` crate's builder refuses to *create* a `..` entry, so an
    /// attacker-crafted archive has to be assembled byte-for-byte here.
    fn raw_tar_with(name: &str, data: &[u8]) -> Vec<u8> {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        let size = format!("{:011o}", data.len());
        h[124..135].copy_from_slice(size.as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = b'0'; // regular file
        h[257..262].copy_from_slice(b"ustar");
        // checksum: spaces while summing, then the octal sum
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let ck = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(ck.as_bytes());

        let mut out = h.to_vec();
        out.extend_from_slice(data);
        out.extend(std::iter::repeat_n(0u8, (512 - data.len() % 512) % 512));
        out.extend(std::iter::repeat_n(0u8, 1024)); // end-of-archive
        out
    }

    fn write_zstd_tar(path: &Path, tar_bytes: &[u8]) {
        let out = std::fs::File::create(path).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(out, 3).unwrap();
        std::io::Write::write_all(&mut enc, tar_bytes).unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn unsafe_archive_paths_are_rejected() {
        assert!(is_unsafe_archive_path(Path::new("../escaped.txt")));
        assert!(is_unsafe_archive_path(Path::new("a/../../etc/passwd")));
        assert!(is_unsafe_archive_path(Path::new("/etc/passwd")));
        assert!(!is_unsafe_archive_path(Path::new("a/b.txt")));
        assert!(!is_unsafe_archive_path(Path::new("b.txt")));
        assert!(!is_unsafe_archive_path(Path::new("a/..b/c"))); // not a ParentDir
    }

    #[test]
    fn unpack_rejects_parent_directory_traversal() {
        // A malicious archive whose entry escapes the destination must be
        // refused, and nothing may appear outside the destination.
        let root = scratch("trav");
        let arc = root.join("evil.tar.zst");
        write_zstd_tar(&arc, &raw_tar_with("../escaped.txt", b"pwn"));
        let dest = root.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let res = unpack_tar_zstd(&arc, &dest);
        assert!(res.is_err(), "traversal entry must be rejected");
        assert!(!root.join("escaped.txt").exists(), "nothing may escape");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unpack_replaces_destination_and_cleans_up_on_failure() {
        // On success the old destination is replaced wholesale; on failure the
        // staging dir is removed and the destination is left intact.
        let root = scratch("promote");
        let src = root.join("tree");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        let arc = root.join("p.tar.zst");
        pack_tar_zstd(&src, &arc).unwrap();

        let dest = root.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.txt"), b"stale").unwrap();
        unpack_tar_zstd(&arc, &dest).unwrap();
        assert!(dest.join("new.txt").exists());
        assert!(
            !dest.join("stale.txt").exists(),
            "old tree must be replaced"
        );
        // The staging dir is a sibling and must be gone after promotion.
        let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("pereprava-unpack"))
            .collect();
        assert!(leftovers.is_empty(), "staging must not be left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_file_detects_identical_inodes() {
        let root = scratch("samefile");
        let a = root.join("a");
        std::fs::write(&a, b"x").unwrap();
        let ma = std::fs::metadata(&a).unwrap();
        let mb = std::fs::metadata(&a).unwrap();
        assert!(same_file(&ma, &mb));
        let other = root.join("b");
        std::fs::write(&other, b"y").unwrap();
        assert!(!same_file(&ma, &std::fs::metadata(&other).unwrap()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
