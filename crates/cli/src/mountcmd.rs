//! `mount` / `unmount` — expose the phone in Finder via a loopback NFSv3
//! volume (ADR-002). Read-only MVP.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use pereprava_core::DeviceHandle;
use pereprava_nfs::MtpNfs;

/// Whether the NFS listener must reject clients with a non-privileged
/// source port.
///
/// **Must stay `false`.** macOS `mount_nfs` always connects from an ephemeral
/// (>= 1024) port, and fernfs drops any such peer when this is enabled
/// (`vendor/fernfs/src/tcp.rs`), which makes every mount hang. Turning it on
/// "for correctness" per RFC 1813 broke mounting on this machine.
const REQUIRE_PRIVILEGED_SOURCE_PORT: bool = false;

/// Mounts the connected device at `path` and serves until Ctrl-C.
///
/// With `serve_only` the NFS server runs without invoking `mount_nfs`
/// (protocol debugging / tests that cannot gain root).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    path: PathBuf,
    port: u16,
    serve_only: bool,
    allow_unprivileged_source_port: bool,
    export: String,
    read_only: bool,
) -> Result<()> {
    let dev = super::commands::connect().await?;
    let nfs = MtpNfs::new(dev.clone(), !read_only)
        .await
        .context("building the NFS view of the device")?;

    let listener =
        pereprava_nfs::fernfs::tcp::NFSTcpListener::bind(&format!("127.0.0.1:{port}"), nfs)
            .await
            .with_context(|| format!("binding NFS server on 127.0.0.1:{port}"))?;
    let mut listener = listener;
    // Unprivileged source ports are REQUIRED here (see the constant).
    // The flag is accepted for CLI compatibility but cannot change this.
    let _ = allow_unprivileged_source_port;
    listener.require_privileged_source_port(REQUIRE_PRIVILEGED_SOURCE_PORT);
    listener.with_export_name(&export);
    let server = tokio::spawn(async move {
        use pereprava_nfs::fernfs::tcp::NFSTcp;
        if let Err(e) = listener.handle_forever().await {
            tracing::error!("nfs server stopped: {e}");
        }
    });

    if serve_only {
        println!("serving NFS on 127.0.0.1:{port} (no mount); Ctrl-C to stop");
        tokio::signal::ctrl_c().await.ok();
        dev.close().await.ok();
        server.abort();
        return Ok(());
    }

    // Retain the path actually mounted: the helper falls back to `-2`, `-3`, …
    // when the requested point is occupied, and unmounting the original path
    // would leave the real mount behind.
    let used = pereprava_nfs::mount_export(port, &path, &export).await?;

    println!(
        "mounted {} ({}) — press Ctrl-C to unmount",
        used.display(),
        if read_only { "read-only" } else { "writable" }
    );
    println!("the volume should now appear in Finder");
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl-C")?;

    println!("unmounting...");
    if let Err(e) = pereprava_nfs::unmount(&used).await {
        eprintln!(
            "warning: unmount failed ({e}); run `sudo umount -f {}` manually",
            used.display()
        );
    }
    dev.close().await.ok();
    server.abort();
    Ok(())
}

/// `unmount` — detach without killing the CLI process tree.
///
/// Standalone helper for when `mount` was interrupted.
pub async fn detach(path: PathBuf) -> Result<()> {
    // Do not gate on `exists()`: a stale NFS mount fails `stat` with ESTALE and
    // `exists()` returns false for exactly the mounts we most need to detach.
    // Try the requested point first, then the `-2`..`-9` fallbacks.
    let mut candidates = vec![path.clone()];
    candidates.extend(pereprava_nfs::mount_candidates(&path).into_iter().skip(1));
    let mut last_err = None;
    for candidate in candidates {
        match pereprava_nfs::unmount(&candidate).await {
            Ok(()) => {
                if candidate != path {
                    println!("unmounted {}", candidate.display());
                }
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no mounted volume found near {}", path.display())))
}

/// `watch` — keep the phone's volume alive across connect/disconnect cycles.
///
/// The NFS listener and the macOS mount point are established once; MTP
/// sessions rotate underneath (`MtpNfs::attach/detach`). Because the adapter
/// generation never changes, kernel filehandles survive every rotation and
/// no additional admin prompts appear after the very first mount.
#[allow(clippy::too_many_arguments)]
pub async fn watch(
    path: PathBuf,
    port: u16,
    read_only: bool,
    poll_secs: u64,
    allow_unprivileged_source_port: bool,
) -> Result<()> {
    let nfs =
        std::sync::Arc::new(MtpNfs::new_detached(!read_only).context("preparing the NFS adapter")?);

    let listener = bind_nfs(port, &nfs, allow_unprivileged_source_port).await?;
    // A previous daemon generation can leave dead NFS mounts behind (base and
    // `-2`..`-9`); without this the new instance is pushed further out every
    // restart. We hold the port, so nothing we can see is serving them.
    clear_stale_mounts(&path).await;
    let server = tokio::spawn(async move {
        use pereprava_nfs::fernfs::tcp::NFSTcp;
        if let Err(e) = listener.handle_forever().await {
            tracing::error!("nfs server stopped: {e}");
        }
    });

    println!("watch: NFS ready on 127.0.0.1:{port}; polling for a phone every {poll_secs}s");

    let poll = std::time::Duration::from_secs(poll_secs);
    let mut mounted = false;
    let mut used_path = String::new();
    let mut last_model = String::new();
    let mut prev_counters = nfs.stats();
    let mut prev_instant = std::time::Instant::now();
    write_status_file("waiting", "", "", 0, 0, 0, 0);
    // launchd stops the job with SIGTERM; without catching it the process dies
    // abruptly and abandons its NFS mount as a stale entry.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing a SIGTERM handler")?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            _ = tokio::time::sleep(poll) => {}
        }

        let mut state = if nfs.is_attached() {
            "attached"
        } else {
            "waiting"
        };

        if !nfs.is_attached() {
            // ptpcamerad re-claims freshly plugged MTP devices within
            // seconds; suppress it around our own connect attempt.
            let _ = tokio::process::Command::new("pkill")
                .args(["-9", "ptpcamerad"])
                .output()
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            match DeviceHandle::connect_first().await {
                Ok(dev) => {
                    let model = dev
                        .info()
                        .await
                        .ok()
                        .and_then(|i| i.product)
                        .unwrap_or_else(|| "unknown".into());
                    if let Err(e) = nfs.attach(dev.clone()).await {
                        eprintln!("attach failed: {e}");
                        drop(dev.close().await);
                    } else {
                        last_model = model;
                        state = "attached";
                        println!("phone attached ({last_model})");
                        if !mounted || !is_alive_mount(std::path::Path::new(&used_path)) {
                            // Our own dead mount may still hold the path.
                            clear_stale_mounts(&path).await;
                            match pereprava_nfs::mount(port, &path).await {
                                Ok(used) => {
                                    mounted = true;
                                    used_path = used.display().to_string();
                                    println!(
                                        "volume mounted at {} — reconnects are prompt-free",
                                        used.display()
                                    );
                                }
                                Err(e) => {
                                    // Back off hard: osascript prompts stack otherwise.
                                    eprintln!(
                                        "mount failed ({e});\n  fix: sudo umount -f {path:?} && \
                                         rerun, or install autorun via \
                                         scripts/install-autorun.sh"
                                    );
                                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::debug!("connect failed: {e}"),
            }
        } else {
            // Liveness = does the SESSION still answer? Bus enumeration lies
            // (charge-only mode, unrelated USB gadgets), the session doesn't.
            if !nfs.test_session().await {
                println!("phone gone: session paused (volume stays mounted)");
                nfs.detach();
                state = "gone";
            }
        }

        // Publish live counters + throughput for the status widget.
        let counters = nfs.stats();
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(prev_instant).as_secs_f64().max(0.001);
        let speed_rx = ((counters.0.saturating_sub(prev_counters.0)) as f64 / elapsed) as u64;
        let speed_tx = ((counters.1.saturating_sub(prev_counters.1)) as f64 / elapsed) as u64;
        prev_counters = counters;
        prev_instant = now;
        write_status_file(
            state,
            &last_model,
            &used_path,
            counters.0,
            counters.1,
            speed_rx,
            speed_tx,
        );
    }

    println!("watch stopped: detaching session and releasing the volume");
    nfs.detach();
    if mounted {
        let mp = std::path::Path::new(&used_path);
        match force_unmount(mp).await {
            Ok(()) => println!("unmounted {}", mp.display()),
            Err(e) => eprintln!("warning: could not unmount {}: {e}", mp.display()),
        }
    }
    write_status_file("stopped", "", "", 0, 0, 0, 0);
    server.abort();
    Ok(())
}

/// Binds the loopback NFS listener, waiting out a port held by another
/// instance rather than failing.
///
/// The port doubles as a singleton lock: with a `KeepAlive` launchd job a hard
/// bind failure would otherwise turn into a crash/restart loop that spams the
/// log and races the live instance.
async fn bind_nfs(
    port: u16,
    nfs: &std::sync::Arc<MtpNfs>,
    allow_unprivileged_source_port: bool,
) -> Result<pereprava_nfs::fernfs::tcp::NFSTcpListener<pereprava_nfs::SharedMtpNfs>> {
    let mut warned = false;
    loop {
        let shared = pereprava_nfs::SharedMtpNfs(nfs.clone());
        match pereprava_nfs::fernfs::tcp::NFSTcpListener::bind(&format!("127.0.0.1:{port}"), shared)
            .await
        {
            Ok(mut listener) => {
                // Unprivileged source ports are required (see the constant).
                let _ = allow_unprivileged_source_port;
                listener.require_privileged_source_port(REQUIRE_PRIVILEGED_SOURCE_PORT);
                return Ok(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if !warned {
                    eprintln!(
                        "watch: port {port} is already served by another pereprava \
                         instance; waiting for it to exit"
                    );
                    warned = true;
                }
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => bail!("interrupted while waiting for port {port}"),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                }
            }
            Err(e) => {
                return Err(e).with_context(|| format!("binding NFS server on 127.0.0.1:{port}"));
            }
        }
    }
}

/// Which of `base`'s candidate paths the kernel currently has mounted.
///
/// Parses `mount(8)` output on purpose: a stale NFS mount answers `stat` and
/// `statfs` with `ESTALE`, so the usual device-number comparison reports
/// "not mounted" for the exact case we need to clean up.
async fn active_mounts(base: &std::path::Path) -> Vec<PathBuf> {
    let Ok(out) = tokio::process::Command::new("/sbin/mount").output().await else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    pereprava_nfs::mount_candidates(base)
        .into_iter()
        .filter(|p| text.contains(&format!(" on {} (", p.display())))
        .collect()
}

/// Force-unmounts `path`, trying `umount(8)` then `diskutil`.
///
/// macOS is notoriously stubborn about stale NFS mounts; `diskutil unmount
/// force` succeeds in some cases where `umount -f` does not.
async fn force_unmount(path: &std::path::Path) -> std::result::Result<(), String> {
    let attempts: [(&str, &[&str]); 2] = [
        ("/sbin/umount", &["-f"]),
        ("/usr/sbin/diskutil", &["unmount", "force"]),
    ];
    let mut last = String::from("no attempt made");
    for (prog, args) in attempts {
        match tokio::process::Command::new(prog)
            .args(args)
            .arg(path)
            .output()
            .await
        {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => last = format!("{prog}: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => last = format!("{prog}: {e}"),
        }
    }
    Err(last)
}

/// Best-effort force-unmount of leftover mounts for `base` and its fallbacks.
///
/// Inside the daemon this runs as root, so it succeeds without any prompt;
/// as a normal user it fails harmlessly and the mount fallback takes over.
/// Arguments are passed as argv, never through a shell.
async fn clear_stale_mounts(base: &std::path::Path) {
    for path in active_mounts(base).await {
        match force_unmount(&path).await {
            Ok(()) => println!("cleared a stale mount at {}", path.display()),
            Err(e) => eprintln!(
                "warning: could not clear stale mount at {}: {e}",
                path.display()
            ),
        }
    }
}

/// True when `path` is a responsive mount point (device differs from its
/// parent). A stale mount fails the metadata lookup and reports `false`, which
/// is what makes the watcher remount after its server dies.
fn is_alive_mount(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("/"));
    match (std::fs::metadata(path), std::fs::metadata(parent)) {
        (Ok(child), Ok(up)) => child.dev() != up.dev(),
        _ => false,
    }
}

/// Current wall-clock time in whole seconds, for status freshness checks.
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Escapes a string for embedding in a JSON string literal (RFC 8259).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Writes the menu-bar status file atomically and safely.
///
/// The daemon runs as root, so a predictable temp file in shared `/tmp` would
/// let any local user pre-create it as a symlink and have the daemon truncate
/// an arbitrary root-owned file. Instead:
///   * the temp file is created in a `0700` directory owned by us,
///   * with `create_new` (fails if the name already exists — no symlink
///     following),
///   * and carries entropy from pid + a monotonically increasing counter.
fn write_status_file(
    state: &str,
    model: &str,
    mounted: &str,
    rx: u64,
    tx: u64,
    speed_rx: u64,
    speed_tx: u64,
) {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let body = format!(
        "{{\"state\":\"{}\",\"model\":\"{}\",\"mounted\":\"{}\",\"rx\":{},\"tx\":{},\"speed_rx\":{},\"speed_tx\":{},\"ts\":{}}}",
        json_escape(state),
        json_escape(model),
        json_escape(mounted),
        rx,
        tx,
        speed_rx,
        speed_tx,
        unix_now_secs()
    );

    let dst = std::path::PathBuf::from("/tmp/pereprava-status.json");
    // A private staging directory keeps the create_new temp out of reach of
    // other users; 0700 is set explicitly (create_dir_all honours umask).
    let dir = std::path::PathBuf::from("/tmp/pereprava-status.d");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).is_err() {
            return;
        }
    }

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{}-{n}", std::process::id()));
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    else {
        return;
    };
    if f.write_all(body.as_bytes()).is_ok() && f.sync_all().is_ok() {
        drop(f);
        // Atomic replace; the public file is world-readable by design.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644));
        }
        if std::fs::rename(&tmp, &dst).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        drop(f);
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_handles_control_and_quote_chars() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn status_json_is_parsable_after_escaping() {
        // A device model containing a quote or backslash must not corrupt the
        // snapshot: the widget parses this with a strict JSON reader. Every
        // raw `"` inside the value must be escaped.
        let model = "A\\065\"x";
        let body = format!(
            "{{\"state\":\"attached\",\"model\":\"{}\"}}",
            json_escape(model)
        );
        assert_eq!(body, r#"{"state":"attached","model":"A\\065\"x"}"#);
    }

    #[test]
    fn is_alive_mount_is_false_for_stale_paths() {
        // A stale NFS mount errors on stat, so this must report false (the
        // watcher then remounts). Use a definitely-stale, absent path.
        assert!(!is_alive_mount(std::path::Path::new("/nonexistent-pv-xyz")));
    }

    /// Regression guard: the strict privileged-source-port mode must never be
    /// turned on, because the macOS NFS client cannot satisfy it.
    #[test]
    fn nfs_listener_allows_ephemeral_source_ports() {
        // Every listener must be configured through the constant, never a
        // literal `true`.
        let src = include_str!("mountcmd.rs");
        let test_start = src.find("mod tests").expect("tests module");
        let prod = &src[..test_start];
        assert!(
            !prod.contains("require_privileged_source_port(true)"),
            "strict mode must never be enabled; use the constant"
        );
        assert_eq!(
            prod.matches("require_privileged_source_port(REQUIRE_PRIVILEGED_SOURCE_PORT)")
                .count(),
            2,
            "both the one-shot mount and the watcher must use the constant"
        );
    }
}
