//! Human-facing formatting helpers.

/// Formats a byte count with an appropriate unit (`KiB`, `MiB`, ...).
#[must_use]
pub fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let b = n as f64;
    if n < 1024 {
        return format!("{n} B");
    }
    if b < K * K {
        return format!("{:.1} KiB", b / K);
    }
    if b < K * K * K {
        return format!("{:.1} MiB", b / (K * K));
    }
    format!("{:.2} GiB", b / (K * K * K))
}

/// Formats a throughput given bytes and elapsed milliseconds.
#[must_use]
pub fn human_rate(bytes: u64, millis: u128) -> String {
    if millis == 0 {
        return "n/a".into();
    }
    let mib = bytes as f64 / (1024.0 * 1024.0);
    let secs = millis as f64 / 1000.0;
    format!("{:.2} MiB/s", mib / secs)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn formats_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn rate_handles_zero_time() {
        assert_eq!(human_rate(1000, 0), "n/a");
    }
}
