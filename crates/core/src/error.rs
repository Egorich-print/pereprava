//! Unified error type for the core crate.
//!
//! The mapping from `mtp-rs` is deliberately *semantic*, not string-based: a
//! caller has to be able to tell "the object is gone" from "the phone
//! unplugged" from "Android re-keyed this handle" from "the disk is full",
//! because those demand completely different reactions (retry, remount, re-list,
//! report). Collapsing them into one `Mtp(String)` is what previously made
//! the NFS layer answer `IO` for everything and the kernel retry forever.

use thiserror::Error;

/// Errors produced by the device actor and helpers.
#[derive(Debug, Error)]
pub enum Error {
    /// The MTP library reported a protocol/transport failure we cannot classify
    /// more precisely.
    #[error("mtp: {0}")]
    Mtp(String),

    /// Requested object or directory does not exist on the device.
    #[error("not found on device: {0}")]
    NotFound(String),

    /// A handle we hold is no longer valid because the device re-keyed it.
    ///
    /// Android's MediaProvider re-assigns object IDs during a media rescan, so
    /// a cached handle can be invalidated while the session is perfectly
    /// healthy. The correct recovery is to re-list the parent, re-resolve the
    /// name and retry once — *not* to treat it as a disconnect and *not* to
    /// report "no such file" to the user.
    #[error("stale object handle (device re-keyed it; re-list and retry): {0}")]
    StaleHandle(String),

    /// Caller supplied an invalid argument (bad path shape, bad storage...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Operation needs a directory but found a file (or vice versa).
    #[error("wrong object kind: {0}")]
    WrongKind(String),

    /// The object or storage is write-protected / access was refused.
    #[error("access denied by device: {0}")]
    AccessDenied(String),

    /// The target storage is full.
    #[error("device storage is full: {0}")]
    StorageFull(String),

    /// The device is temporarily busy; the same call may succeed shortly.
    #[error("device busy: {0}")]
    Busy(String),

    /// The device does not implement the requested operation.
    #[error("unsupported by device: {0}")]
    Unsupported(String),

    /// Another process holds the device exclusively (e.g. `ptpcamerad`).
    #[error("device is held exclusively by another process")]
    ExclusiveAccess,

    /// Local filesystem I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The actor channel is closed (actor task died).
    #[error("device actor is not running")]
    ActorClosed,

    /// USB link lost mid-operation — the phone is physically gone and a
    /// reconnect is required.
    #[error("device disconnected")]
    Disconnected,

    /// The session died but the device is still plugged in (MTP reset after a
    /// transfer wedge). Reopening is enough; no replug needed.
    #[error("device session was reset; reopen the device")]
    SessionReset,

    /// An operation exceeded its deadline.
    #[error("device operation timed out")]
    Timeout,
}

impl From<mtp_rs::Error> for Error {
    fn from(e: mtp_rs::Error) -> Self {
        // Order matters: `StaleHandle` and `DeviceReset` used to be folded
        // into `Disconnected`, which told the watcher the phone was gone when
        // it was merely re-keyed or had a reset session.
        match e {
            mtp_rs::Error::NotFound => Error::NotFound(String::new()),
            mtp_rs::Error::StaleHandle => Error::StaleHandle(String::new()),
            mtp_rs::Error::AccessDenied => Error::AccessDenied(String::new()),
            mtp_rs::Error::StorageFull => Error::StorageFull(String::new()),
            mtp_rs::Error::Busy => Error::Busy(String::new()),
            mtp_rs::Error::Unsupported => Error::Unsupported(String::new()),
            mtp_rs::Error::ExclusiveAccess => Error::ExclusiveAccess,
            mtp_rs::Error::Disconnected => Error::Disconnected,
            mtp_rs::Error::DeviceReset => Error::SessionReset,
            mtp_rs::Error::Timeout => Error::Timeout,
            // `NoDevice`, `Cancelled`, `InvalidData`, `Io`, `Other` and any
            // future variant keep their detail and stay "protocol" errors.
            other => Error::Mtp(other.to_string()),
        }
    }
}

impl Error {
    /// Builds [`Error::Mtp`] from any displayable MTP failure.
    pub fn mtp_msg(msg: impl std::fmt::Display) -> Self {
        Error::Mtp(msg.to_string())
    }

    /// True when the operation may succeed if retried unchanged.
    ///
    /// Used by the watcher to avoid tearing down a healthy session on a
    /// transient `Busy`/`Timeout`.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Busy(_) | Error::Timeout)
    }

    /// True when the USB link itself is gone and a reconnect is required.
    #[must_use]
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Error::Disconnected)
    }

    /// True when this handle can be recovered by re-resolving the name.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        matches!(self, Error::StaleHandle(_))
    }
}

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn mtp_msg_formats_display() {
        let e = Error::mtp_msg("boom 42");
        assert!(matches!(e, Error::Mtp(ref s) if s == "boom 42"));
    }

    #[test]
    fn not_found_carries_path() {
        let e = Error::NotFound("/a/b".into());
        assert!(e.to_string().contains("/a/b"));
    }

    #[test]
    fn stale_handle_is_not_a_disconnect() {
        // Regression: Android re-keys handles on a media rescan. Reporting
        // "disconnected" made the watcher drop a perfectly good session and
        // told the user to replug the phone.
        let e: Error = mtp_rs::Error::StaleHandle.into();
        assert!(e.is_stale(), "must be recoverable by re-listing");
        assert!(!e.is_disconnect(), "the phone is still connected");
        assert!(!e.is_retryable(), "an unchanged retry will not help");
    }

    #[test]
    fn disconnect_is_a_disconnect_only() {
        let e: Error = mtp_rs::Error::Disconnected.into();
        assert!(e.is_disconnect());
        assert!(!e.is_stale());
    }

    #[test]
    fn session_reset_is_neither_disconnect_nor_stale() {
        // The phone is plugged in and the session was merely reset: reopen it.
        let e: Error = mtp_rs::Error::DeviceReset.into();
        assert!(!e.is_disconnect());
        assert!(!e.is_stale());
        assert!(matches!(e, Error::SessionReset));
    }

    #[test]
    fn not_found_maps_to_not_found() {
        let e: Error = mtp_rs::Error::NotFound.into();
        assert!(matches!(e, Error::NotFound(_)));
    }

    #[test]
    fn busy_and_timeout_are_retryable() {
        let busy: Error = mtp_rs::Error::Busy.into();
        let timeout: Error = mtp_rs::Error::Timeout.into();
        assert!(busy.is_retryable());
        assert!(timeout.is_retryable());
    }

    #[test]
    fn terminal_errors_are_not_retryable() {
        for e in [
            Error::StorageFull(String::new()),
            Error::AccessDenied(String::new()),
            Error::Disconnected,
            Error::NotFound(String::new()),
        ] {
            assert!(!e.is_retryable(), "{e:?} must not be retried blindly");
        }
    }
    #[test]
    fn unclassified_variants_keep_their_detail() {
        let e: Error = mtp_rs::Error::Other {
            detail: "0x2002 GeneralError".into(),
        }
        .into();
        match e {
            Error::Mtp(s) => assert!(s.contains("GeneralError")),
            other => panic!("expected Mtp, got {other:?}"),
        }
    }
}
