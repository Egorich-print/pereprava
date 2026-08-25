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
    let counts = tokio::task::spawn_blocking(move || pack_tar_zstd(&src, &dst))
        .await
        .context("pack task panicked")?
        .context("packing failed")?;

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
    let counts = tokio::task::spawn_blocking(move || unpack_tar_zstd(&arc, &dst))
        .await
        .context("unpack task panicked")?
        .context("extraction failed")?;
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
    walk_add(&mut builder, src, src, &mut files, &mut dirs, &mut raw)?;
    builder.finish()?;
    Ok((files, dirs, raw))
}

fn walk_add(
    builder: &mut tar::Builder<impl Write>,
    root: &Path,
    dir: &Path,
    files: &mut u32,
    dirs: &mut u32,
    raw: &mut u64,
) -> std::io::Result<()> {
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
        if path.is_dir() {
            *dirs += 1;
            builder.append_dir(rel, &path)?;
            walk_add(builder, root, &path, files, dirs, raw)?;
        } else {
            *files += 1;
            *raw += path.metadata().map_err(std::io::Error::other)?.len();
            builder.append_path_with_name(&path, rel)?;
        }
    }
    Ok(())
}

/// Extracts a .tar.zst archive. Returns (files, dirs, raw bytes).
fn unpack_tar_zstd(archive: &Path, dest: &Path) -> std::io::Result<(u32, u32, u64)> {
    let f = std::fs::File::open(archive)?;
    let dec = zstd::stream::read::Decoder::new(f)?;
    let mut ar = tar::Archive::new(dec);
    ar.set_preserve_permissions(false);
    ar.set_unpack_xattrs(false);

    let mut files = 0u32;
    let mut dirs = 0u32;
    let mut raw = 0u64;
    for entry in ar.entries()? {
        let mut e = entry?;
        match e.header().entry_type() {
            tar::EntryType::Directory => dirs += 1,
            tar::EntryType::Regular => {
                files += 1;
                raw += e.size();
            }
            _ => continue, // symlinks/devices are not part of our bundles
        }
        e.unpack_in(dest)?;
    }
    Ok((files, dirs, raw))
}

fn silent() -> tokio::sync::watch::Sender<pereprava_core::Progress> {
    tokio::sync::watch::channel(pereprava_core::Progress { total: 0, done: 0 }).0
}
