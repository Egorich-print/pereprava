//! `mount` / `unmount` — expose the phone in Finder via a loopback NFSv3
//! volume (ADR-002). Read-only MVP.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use pereprava_core::DeviceHandle;
use pereprava_nfs::MtpNfs;

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
    if allow_unprivileged_source_port {
        listener.require_privileged_source_port(false);
    }
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

    pereprava_nfs::mount(port, &path).await?;

    println!(
        "mounted {} (read-only) — press Ctrl-C to unmount",
        path.display()
    );
    println!("the volume should now appear in Finder");
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl-C")?;

    println!("unmounting...");
    if let Err(e) = pereprava_nfs::unmount(&path).await {
        eprintln!(
            "warning: unmount failed ({e}); run `sudo umount -f {:?}` manually",
            path
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
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    pereprava_nfs::unmount(&path).await
}

/// `watch` — keep the phone's volume alive across connect/disconnect cycles.
///
/// The NFS listener and the macOS mount point are established once; MTP
/// sessions rotate underneath (`MtpNfs::attach/detach`). Because the adapter
/// generation never changes, kernel filehandles survive every rotation and
/// no additional admin prompts appear after the very first mount.
#[allow(clippy::too_many_arguments)]
pub async fn watch(path: PathBuf, port: u16, read_only: bool, poll_secs: u64) -> Result<()> {
    let nfs =
        std::sync::Arc::new(MtpNfs::new_detached(!read_only).context("preparing the NFS adapter")?);

    let listener = bind_nfs(port, &nfs).await?;
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
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
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
                        if !mounted || !is_mounted(std::path::Path::new(&used_path)) {
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

    println!("watch stopped: detaching session (volume left as-is)");
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
) -> Result<pereprava_nfs::fernfs::tcp::NFSTcpListener<pereprava_nfs::SharedMtpNfs>> {
    let mut warned = false;
    loop {
        let shared = pereprava_nfs::SharedMtpNfs(nfs.clone());
        match pereprava_nfs::fernfs::tcp::NFSTcpListener::bind(&format!("127.0.0.1:{port}"), shared)
            .await
        {
            Ok(listener) => return Ok(listener),
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

/// True when `path` is an active mount point: on macOS a mounted filesystem
/// reports a device different from its parent directory.
fn is_mounted(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("/"));
    match (std::fs::metadata(path), std::fs::metadata(parent)) {
        (Ok(child), Ok(up)) => child.dev() != up.dev(),
        _ => false,
    }
}

/// Writes the menu-bar status file atomically.
fn write_status_file(
    state: &str,
    model: &str,
    mounted: &str,
    rx: u64,
    tx: u64,
    speed_rx: u64,
    speed_tx: u64,
) {
    let esc = |s: &str| s.replace('"', "\\\"");
    let body = format!(
        "{{\"state\":\"{}\",\"model\":\"{}\",\"mounted\":\"{}\",\"rx\":{},\"tx\":{},\"speed_rx\":{},\"speed_tx\":{}}}",
        esc(state),
        esc(model),
        esc(mounted),
        rx,
        tx,
        speed_rx,
        speed_tx
    );
    let dst = std::path::PathBuf::from("/tmp/pereprava-status.json");
    let tmp = dst.with_extension("json.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        drop(std::fs::rename(&tmp, &dst));
    }
}
