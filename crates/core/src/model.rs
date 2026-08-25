//! Value types shared between the actor and front-ends (phase 2a).

/// A file or directory entry reported by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Object handle assigned by the device (MTP object id).
    pub handle: u32,
    /// Parent object handle (`u32::MAX` for storage roots).
    pub parent: u32,
    /// Display name.
    pub name: String,
    /// True when the entry is a directory.
    pub is_dir: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
}
