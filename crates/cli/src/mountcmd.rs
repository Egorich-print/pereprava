//! `mount` / `unmount` — expose the phone in Finder via a loopback NFSv3
//! volume (ADR-002). Read-only MVP.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
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
