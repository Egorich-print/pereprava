//! NFSv3 loopback mount backend exposing an MTP device through [`fernfs`].
//!
//! - `adapter` — `MtpNfs`: the VFS implementation mapping NFS ids onto MTP
//!   object handles (read-only MVP per ADR-002).
//! - `mount` — macOS `mount_nfs` / `umount` automation.

pub mod adapter;
pub mod mount;

/// Re-export so downstream code can reach `fernfs::tcp` etc. without adding
/// the vendored path dependency itself.
pub use fernfs;

pub use adapter::MtpNfs;
pub use mount::{mount, unmount};
