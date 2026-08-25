//! Unified error type for the core crate.

use thiserror::Error;

/// Errors produced by the device actor and helpers.
#[derive(Debug, Error)]
pub enum Error {
    /// The MTP library reported a protocol/transport failure.
    #[error("mtp: {0}")]
    Mtp(String),

    /// Requested object or directory does not exist on the device.
    #[error("not found on device: {0}")]
    NotFound(String),

    /// Caller supplied an invalid argument (bad path shape, bad storage...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Target exists where uniqueness was required.
    #[error("already exists on device: {0}")]
    AlreadyExists(String),

    /// Operation needs a directory but found a file (or vice versa).
    #[error("wrong object kind: {0}")]
    WrongKind(String),

    /// Local filesystem I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The actor channel is closed (actor task died).
    #[error("device actor is not running")]
    ActorClosed,
}

impl Error {
    /// Builds [`Error::Mtp`] from any displayable MTP failure.
    pub fn mtp_msg(msg: impl std::fmt::Display) -> Self {
        Error::Mtp(msg.to_string())
    }
}

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
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
}
