//! pereprava-core: async MTP device access layer.
//!
//! The crate wraps [`mtp_rs`] behind a single-task *device actor* so that all
//! protocol traffic is serialized exactly the way MTP likes it, while callers
//! get a cheap cloneable handle with request/response semantics.

pub mod actor;
pub mod error;
pub mod model;
