//! Value types shared between the device actor and front-ends.

/// A file or directory entry reported by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Object handle assigned by the device (MTP object id).
    pub handle: u64,
    /// Parent object handle (`0` = storage root level).
    pub parent: u64,
    /// Display name.
    pub name: String,
    /// True when the entry is a directory.
    pub is_dir: bool,
    /// Size in bytes (always `0` for directories).
    pub size: u64,
}

/// One storage volume exposed by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageSummary {
    /// Raw MTP storage id.
    pub id: u32,
    /// Human-readable description, e.g. `"Internal shared storage"`.
    pub description: String,
    /// Total capacity in bytes.
    pub capacity: u64,
    /// Free space in bytes.
    pub free: u64,
    /// Whether the storage accepts writes.
    pub writable: bool,
}

/// Static description of the connected device plus its volumes.
#[derive(Debug, Clone)]
pub struct DeviceSummary {
    /// USB vendor id (`0` when only session-level info is available).
    pub vendor_id: u16,
    /// USB product id (`0` when only session-level info is available).
    pub product_id: u16,
    /// Manufacturer string, when the device reports one.
    pub manufacturer: Option<String>,
    /// Product/model string, when the device reports one.
    pub product: Option<String>,
    /// Serial number, when the device reports one.
    pub serial: Option<String>,
    /// Negotiated USB link speed label, e.g. `"High (480 Mbit/s, USB 2.0)"`.
    pub speed: Option<String>,
    /// Firmware version string, when reported.
    pub firmware: Option<String>,
    /// Storages exposed by the device, in enumeration order.
    pub storages: Vec<StorageSummary>,
}

/// Live transfer progress published over a `watch` channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Total bytes planned for this transfer (best estimate).
    pub total: u64,
    /// Bytes transferred so far.
    pub done: u64,
}

impl Progress {
    /// Formats progress as a stable-width percentage string.
    #[must_use]
    pub fn pct(&self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        (self.done.min(self.total) as f64 / self.total as f64) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_pct_handles_zero_total() {
        let p = Progress { total: 0, done: 0 };
        assert!((p.pct() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn progress_pct_saturates() {
        let p = Progress { total: 10, done: 99 };
        assert!((p.pct() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn entry_is_dir_defaults_size_zero_semantics() {
        let e = Entry {
            handle: 1,
            parent: 0,
            name: "DCIM".into(),
            is_dir: true,
            size: 0,
        };
        assert!(e.is_dir && e.size == 0);
    }
}
