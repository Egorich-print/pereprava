//! pereprava-core: async MTP device access layer.
//!
//! The crate wraps [`mtp_rs`] behind a single-task *device actor* so that all
//! protocol traffic is serialized exactly the way MTP likes it, while callers
//! get a cheap cloneable handle with request/response semantics.

pub mod actor;
pub mod cache;
pub mod error;
pub mod model;
pub mod names;
pub mod ops;
pub mod path;

pub use actor::{DeviceHandle, ProbeDevice, Resolved};
pub use error::{Error, Result};
pub use model::{DeviceSummary, Entry, Progress, StorageSummary};
pub use names::{names_eq, names_eq_ci};
