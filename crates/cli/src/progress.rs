//! Live transfer progress rendering on stderr.

use std::time::{Duration, Instant};

use pereprava_core::Progress;
use tokio::sync::watch;

const REFRESH: Duration = Duration::from_millis(150);

/// Spawns a task that renders `label` progress until the sender drops.
pub fn spawn_progress(
    label: String,
    mut rx: watch::Receiver<Progress>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = Instant::now();
        let mut last_paint = Instant::now() - REFRESH;

        while let Ok(()) = rx.changed().await {
            if last_paint.elapsed() >= REFRESH {
                last_paint = Instant::now();
                paint(&label, &rx.borrow().clone(), start);
            }
        }
        paint(&label, &rx.borrow().clone(), start);
        eprintln!();
    })
}

fn paint(label: &str, p: &Progress, start: Instant) {
    let elapsed_ms = start.elapsed().as_millis();
    let rate = if elapsed_ms > 0 {
        p.done as f64 / (elapsed_ms as f64 / 1000.0) / (1024.0 * 1024.0)
    } else {
        0.0
    };
    eprint!(
        "\r{label:<28} {:>6.1}% {:>9} / {:<9} {:>7.1} MiB/s ",
        p.pct(),
        crate::format::human_bytes(p.done),
        crate::format::human_bytes(p.total),
        rate,
    );
}
