//! Unified error type for the core crate.

use thiserror::Error;

/// Errors produced by the device actor and helpers.
#[derive(Debug, Error)]
pub enum Error {
    /// The MTP library reported a failure.
    #[error("mtp: {0}")]
    Mtp(String),

    /// Requested object or directory does not exist on the device.
    #[error("not found on device: {0}")]
    NotFound(String),

    /// Caller supplied an invalid argument (bad path, bad id...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Local filesystem I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The actor channel is closed (actor task died).
    #[error("device actor is not running")]
    ActorClosed,
}

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;
