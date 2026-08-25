//! Device-gated integration tests against a real MTP phone.
//!
//! Run manually (Nothing Phone (2a) used during development):
//!
//! ```shell
//! PEREPRAVA_DEVICE=1 cargo test -p pereprava-core --test device -- --ignored --test-threads=1
//! ```
//!
//! The tests create and remove their own `/1/pereprava-test-<stamp>` tree.

// Test bodies intentionally use panic!/unwrap-style assertions.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;

use pereprava_core::{DeviceHandle, Progress};
use tokio::sync::watch;

fn gate_open() -> bool {
    match std::env::var("PEREPRAVA_DEVICE") {
        Ok(v) => v != "0",
        Err(_) => false,
    }
}

/// Owned in-memory [`tokio::io::AsyncWrite`] sink (`Box<dyn ...>` needs 'static).
/// Clonable handle keeps access to collected bytes after the box takes ownership.
#[derive(Clone, Default)]
struct MemSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl MemSink {
    fn snapshot(&self) -> Vec<u8> {
        self.0.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl tokio::io::AsyncWrite for MemSink {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::io::Write as _;
        match self.0.lock() {
            Ok(mut guard) => guard
                .write_all(buf)
                .map(|_| std::task::Poll::Ready(Ok(buf.len())))
                .unwrap_or_else(|e| std::task::Poll::Ready(Err(e))),
            Err(_) => std::task::Poll::Ready(Err(std::io::Error::other("mem sink mutex poisoned"))),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

async fn silent_progress() -> watch::Sender<Progress> {
    watch::channel(Progress { total: 0, done: 0 }).0
}

#[tokio::test]
#[ignore = "requires a real MTP device; set PEREPRAVA_DEVICE=1 and pass --ignored"]
async fn full_roundtrip_on_device() {
    if !gate_open() {
        eprintln!("skipping: PEREPRAVA_DEVICE is not set");
        return;
    }
    let dev = DeviceHandle::connect_first()
        .await
        .unwrap_or_else(|e| panic!("connect failed: {e}"));

    // --- info ----------------------------------------------------------
    let summary = dev
        .info()
        .await
        .unwrap_or_else(|e| panic!("info failed: {e}"));
    assert!(!summary.storages.is_empty(), "device exposes no storages");

    // --- workspace -----------------------------------------------------
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let base = format!("/1/pereprava-test-{stamp}");
    dev.mkdir_all(&base)
        .await
        .unwrap_or_else(|e| panic!("mkdir failed: {e}"));

    dev.mkdir_all(&format!("{base}/nested"))
        .await
        .unwrap_or_else(|e| panic!("mkdir nested failed: {e}"));

    // --- upload ---------------------------------------------------------
    let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
    let size = payload.len() as u64;
    let entry = dev
        .upload_new(
            &base,
            "roundtrip.bin",
            size,
            Box::new(Cursor::new(payload.clone())),
            silent_progress().await,
        )
        .await
        .unwrap_or_else(|e| panic!("upload failed: {e}"));
    assert_eq!(entry.size, size, "device reports wrong size");

    // --- metadata -------------------------------------------------------
    let resolved = dev
        .resolve(&format!("{base}/roundtrip.bin"))
        .await
        .unwrap_or_else(|e| panic!("resolve failed: {e}"));
    assert_eq!(resolved.entry.size, size);

    // --- listing sees the file -------------------------------------------
    let listing = dev
        .list(&base, true)
        .await
        .unwrap_or_else(|e| panic!("list failed: {e}"));
    assert!(
        listing.iter().any(|e| e.name == "roundtrip.bin"),
        "uploaded file missing from listing"
    );

    // --- download roundtrip ----------------------------------------------
    let sink = MemSink::default();
    let got = dev
        .download_into(
            &format!("{base}/roundtrip.bin"),
            Box::new(sink.clone()),
            silent_progress().await,
        )
        .await
        .unwrap_or_else(|e| panic!("download failed: {e}"));
    let back = sink.snapshot();
    assert_eq!(got, size);
    assert_eq!(back.len() as u64, size, "downloaded byte count differs");
    assert_eq!(back, payload, "payload corrupted over MTP");

    // --- rename -----------------------------------------------------------
    dev.rename(&format!("{base}/roundtrip.bin"), "renamed.bin")
        .await
        .unwrap_or_else(|e| panic!("rename failed: {e}"));
    let moved_listing = dev
        .list(&base, true)
        .await
        .unwrap_or_else(|e| panic!("post-rename listing failed: {e}"));
    assert!(moved_listing.iter().any(|e| e.name == "renamed.bin"));
    assert!(!moved_listing.iter().any(|e| e.name == "roundtrip.bin"));

    // --- cross-directory move ----------------------------------------------
    dev.move_into(&format!("{base}/renamed.bin"), &format!("{base}/nested"))
        .await
        .unwrap_or_else(|e| panic!("move failed: {e}"));
    let nested = dev
        .list(&format!("{base}/nested"), true)
        .await
        .unwrap_or_else(|e| panic!("nested listing failed: {e}"));
    assert!(nested.iter().any(|e| e.name == "renamed.bin"));

    // --- cleanup -------------------------------------------------------------
    let removed = dev
        .remove(&base, true)
        .await
        .unwrap_or_else(|e| panic!("recursive remove failed: {e}"));
    // base + nested + renamed.bin = 3 objects minimum
    assert!(removed >= 3, "expected at least 3 deletions, got {removed}");

    dev.close()
        .await
        .unwrap_or_else(|e| panic!("close failed: {e}"));
}
