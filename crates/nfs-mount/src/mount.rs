//! Mount automation: run the NFS server and attach it with macOS `mount_nfs`.
//!
//! Mounting requires administrator rights. We try the direct path first
//! (works when already root) and otherwise ask via `osascript`, which pops
//! the system GUI password dialog — once per mount, for the whole fallback
//! sequence.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Marker the mount script prints so we can find the winning path even when
/// `mount_nfs` decides to write something to stdout.
const MOUNTED_MARKER: &str = "PEREPRAVA_MOUNTED=";

/// POSIX single-quote escaping: safe for arbitrary paths, including `'`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escapes a script so it can sit inside an AppleScript double-quoted string.
fn osascript_escape(script: &str) -> String {
    script.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Mount points to try in order: the requested path, then `<name>-2`..`-9`.
///
/// Stale/hung mounts left by earlier runs occupy the base path and cannot be
/// removed without root, so the watcher falls back instead of wedging.
///
/// Public so callers can also enumerate (and clean up) mounts a previous
/// generation may have left behind.
#[must_use]
pub fn mount_candidates(mount_point: &Path) -> Vec<PathBuf> {
    let mut out = vec![mount_point.to_path_buf()];
    let name = mount_point
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "pereprava".into());
    let parent = mount_point.parent().unwrap_or_else(|| Path::new("/"));
    for n in 2..=9 {
        out.push(parent.join(format!("{name}-{n}")));
    }
    out
}

/// Attaches `127.0.0.1:/` near `mount_point` using NFSv3 over TCP loopback.
///
/// Returns the path actually used. The whole fallback sequence runs in a
/// single privileged invocation, so at most one admin prompt appears.
///
/// # Errors
/// Fails when every candidate path is exhausted or authorization is denied.
pub async fn mount(port: u16, mount_point: &Path) -> Result<PathBuf> {
    mount_export(port, mount_point, "/").await
}

/// [`mount`], but for a server configured with a non-default export subpath.
///
/// The source passed to `mount_nfs` must match the export the listener was
/// started with, otherwise MNT resolution asks for a path the server does not
/// export.
pub async fn mount_export(port: u16, mount_point: &Path, export: &str) -> Result<PathBuf> {
    // Safety: refuse to mount over the system root or a relative path. The
    // daemon runs as root and force-unmounts "stale" mounts, so an accidental
    // `--path /` would be destructive.
    if mount_point == Path::new("/") {
        bail!("refusing to use `/` as a mount point");
    }
    if !mount_point.is_absolute() {
        bail!(
            "mount point must be an absolute path: {}",
            mount_point.display()
        );
    }
    // Normalize to a leading-slash path with no trailing slash.
    let export = if export.is_empty() || export == "/" {
        "/".to_string()
    } else if export.starts_with('/') {
        export.trim_end_matches('/').to_string()
    } else {
        format!("/{}", export.trim_end_matches('/'))
    };
    let source = sh_quote(&format!("127.0.0.1:{export}"));
    let mut script = String::new();
    for c in mount_candidates(mount_point) {
        let mp = sh_quote(&c.display().to_string());
        // `soft` keeps a dead phone from wedging the volume permanently
        // (hard NFS + vanished USB = unkillable mount); retries stay modest.
        //
        // rsize/wsize are deliberately 1 MiB: each NFS READ is one MTP
        // GetPartialObject transaction and Android's per-transaction overhead
        // dominates at small sizes. The server advertises the same maximum
        // (see `PREFERRED_IO_SIZE` in the adapter).
        script.push_str(&format!(
            "mkdir -p {mp} && /sbin/mount_nfs -o \
             soft,nolocks,vers=3,tcp,rsize=1048576,wsize=1048576,retry=1,retrans=2,timeo=50,\
             port={port},mountport={port} {source} {mp} && \
             {{ printf '{MOUNTED_MARKER}%s\\n' {mp}; exit 0; }}\n"
        ));
    }
    script.push_str("exit 1\n");

    if let Ok(out) = sh_out("/bin/sh", &["-c", &script]).await
        && let Some(path) = mounted_path(&out)
    {
        return Ok(path);
    }
    // Not root: one system dialog covers the entire sequence.
    let escaped = osascript_escape(&script);
    let out = sh_out(
        "/usr/bin/osascript",
        &[
            "-e",
            &format!("do shell script \"{escaped}\" with administrator privileges"),
        ],
    )
    .await
    .with_context(|| format!("mounting near {}", mount_point.display()))?;
    mounted_path(&out).with_context(|| format!("mounted but {MOUNTED_MARKER} was not reported"))
}

/// Extracts the mounted path from a successful script's stdout.
fn mounted_path(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix(MOUNTED_MARKER))
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Detaches `mount_point`, forcing the unmount for hung/crashed servers.
///
/// # Errors
/// Fails when the volume is still busy and authorization is denied.
pub async fn unmount(mount_point: &Path) -> Result<()> {
    let mp = sh_quote(&mount_point.display().to_string());
    // `-f` recovers a mount whose NFS server has gone away; plain umount
    // first so a healthy volume is detached cleanly.
    let script = format!("/sbin/umount {mp} 2>/dev/null || /sbin/umount -f {mp}");
    if sh_out("/bin/sh", &["-c", &script]).await.is_ok() {
        return Ok(());
    }
    let escaped = osascript_escape(&script);
    sh_out(
        "/usr/bin/osascript",
        &[
            "-e",
            &format!("do shell script \"{escaped}\" with administrator privileges"),
        ],
    )
    .await
    .with_context(|| format!("unmounting {}", mount_point.display()))?;
    Ok(())
}

async fn sh_out(prog: &str, args: &[&str]) -> Result<String> {
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
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn quote_handles_single_quotes() {
        assert_eq!(sh_quote("/Volumes/a'b"), "'/Volumes/a'\\''b'");
    }

    #[test]
    fn fallback_candidates_are_numbered() {
        let c = mount_candidates(Path::new("/Volumes/pereprava"));
        assert_eq!(c[0], PathBuf::from("/Volumes/pereprava"));
        assert_eq!(c[1], PathBuf::from("/Volumes/pereprava-2"));
        assert_eq!(c.last().unwrap(), &PathBuf::from("/Volumes/pereprava-9"));
    }

    #[test]
    fn parses_mounted_marker_among_noise() {
        let out = "noise\nPEREPRAVA_MOUNTED=/Volumes/pereprava-3\n";
        assert_eq!(
            mounted_path(out).unwrap(),
            PathBuf::from("/Volumes/pereprava-3")
        );
        assert!(mounted_path("nothing here").is_none());
    }
}
