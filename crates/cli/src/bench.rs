//! `bench` — throughput micro-benchmarks against the connected device.
//!
//! Measures: big-file push/pull MiB/s, many-small-files push, directory
//! listing latency. All artifacts are cleaned up afterwards.

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use pereprava_core::ops;

/// Bench parameters.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Big-file size in MiB.
    pub size_mib: u64,
    /// Number of small files for the metadata-heavy phase.
    pub small_files: u32,
}

/// Runs the benchmark suite and prints a report table.
pub async fn run(params: Params) -> Result<()> {
    super::commands::with_device(|dev| async move { bench_inner(&dev, params).await }).await
}

async fn bench_inner(dev: &pereprava_core::DeviceHandle, params: Params) -> Result<()> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let remote_base = format!("/1/pereprava-bench-{stamp}");
    let local_base: PathBuf = std::env::temp_dir().join(format!("pereprava-bench-{stamp}"));
    tokio::fs::create_dir_all(&local_base).await?;
    dev.mkdir_all(&remote_base).await?;

    println!(
        "pereprava bench — {} MiB file, {} small files",
        params.size_mib, params.small_files
    );
    println!("remote: {remote_base}");

    let mut report = String::new();

    // --- Phase 1: single big file -------------------------------------
    let big_name = "big.bin";
    let big_local = local_base.join(big_name);
    let big_len = params.size_mib * 1024 * 1024;
    let sum_src = write_test_file(&big_local, big_len).await?;

    if params.size_mib > 0 {
        let meta = tokio::fs::metadata(&big_local).await?;
        let f = tokio::fs::File::open(&big_local).await?;
        let t = Instant::now();
        dev.upload_new(&remote_base, big_name, meta.len(), Box::new(f), silent())
            .await?;
        let ms = t.elapsed().as_millis();
        println!(
            "push  {:>8} : {}",
            human_size(big_len),
            crate::format::human_rate(big_len, ms)
        );
        report.push_str(&format!(
            "| big push | {} | {} |\n",
            human_size(big_len),
            crate::format::human_rate(big_len, ms)
        ));

        let remote_big = format!("{remote_base}/{big_name}");
        let out = tokio::fs::File::create(local_base.join("big.out")).await?;
        let t = Instant::now();
        dev.download_into(&remote_big, Box::new(out), silent())
            .await?;
        let ms = t.elapsed().as_millis();
        println!(
            "pull  {:>8} : {}",
            human_size(big_len),
            crate::format::human_rate(big_len, ms)
        );
        report.push_str(&format!(
            "| big pull | {} | {} |\n",
            human_size(big_len),
            crate::format::human_rate(big_len, ms)
        ));

        let sum_back = fnv_file(&local_base.join("big.out")).await?;
        if sum_back != sum_src {
            anyhow::bail!("checksum mismatch after roundtrip ({sum_src:#x} != {sum_back:#x})");
        }
        println!("roundtrip checksum ok ({sum_src:#018x})");
    }

    // --- Phase 2: many small files ------------------------------------
    if params.small_files > 0 {
        let small_dir = local_base.join("small");
        tokio::fs::create_dir_all(&small_dir).await?;
        let mut buf = vec![0u8; 8 * 1024];
        let mut seed: u64 = 0x5EED_B00F;
        xorshift_fill(&mut buf, &mut seed);
        for i in 0..params.small_files {
            tokio::fs::write(small_dir.join(format!("f{i:05}.dat")), &buf).await?;
        }

        let t = Instant::now();
        let stats = ops::push_tree(dev, &small_dir, &remote_base).await?;
        let ms = t.elapsed().as_millis();
        let per_file = ms as f64 / f64::from(stats.files.max(1));
        println!(
            "push  {:>8} x {} : {:.2} s total, {per_file:.2} ms/file",
            "8KiB",
            stats.files,
            ms as f64 / 1000.0
        );
        report.push_str(&format!(
            "| small push | {} x 8KiB | {per_file:.2} ms/file |\n",
            stats.files
        ));

        let remote_small = format!("{remote_base}/small");
        let t = Instant::now();
        let entries = dev.list(&remote_small, false).await?;
        let ms = t.elapsed().as_millis();
        println!(
            "list  {:>8} : {} entries in {ms} ms",
            "readdir",
            entries.len()
        );
        report.push_str(&format!("| list {} entries | {ms} ms |\n", entries.len()));
    }

    // --- Cleanup --------------------------------------------------------
    drop(tokio::fs::remove_dir_all(&local_base).await);
    match dev.remove(&remote_base, true).await {
        Ok(n) => println!("cleanup: removed {n} remote object(s)"),
        Err(e) => eprintln!("warning: remote cleanup failed ({e}); leftover at {remote_base}"),
    }

    println!(
        "\nMarkdown rows (paste into docs/benchmarks/):\n{}",
        report.trim_end()
    );
    Ok(())
}

fn silent() -> tokio::sync::watch::Sender<pereprava_core::Progress> {
    tokio::sync::watch::channel(pereprava_core::Progress { total: 0, done: 0 }).0
}

fn human_size(bytes: u64) -> String {
    crate::format::human_bytes(bytes)
}

/// Writes `len` bytes of deterministic pseudo-random data; returns FNV-1a64.
async fn write_test_file(path: &PathBuf, len: u64) -> Result<u64> {
    use std::io::Write as _;
    let mut f = std::fs::File::create(path).with_context(|| path.display().to_string())?;
    let mut chunk = vec![0u8; 1024 * 1024];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    let mut left = len;
    while left > 0 {
        let n = chunk.len().min(left as usize);
        xorshift_fill(&mut chunk[..n], &mut state);
        hash = fnv_update(hash, &chunk[..n]);
        f.write_all(&chunk[..n])?;
        left -= n as u64;
    }
    f.sync_all()?;
    Ok(hash)
}

fn xorshift_fill(buf: &mut [u8], seed: &mut u64) {
    let mut x = *seed | 1;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    *seed = x;
}

async fn fnv_file(path: &PathBuf) -> Result<u64> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hash = fnv_update(hash, &buf[..n]);
    }
    Ok(hash)
}

fn fnv_update(mut hash: u64, data: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    for b in data {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
