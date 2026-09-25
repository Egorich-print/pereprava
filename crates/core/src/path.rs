//! Device path parsing: `/storage-name/dir/subdir/file.ext`
//!
//! The first segment addresses a storage either by its description
//! (case-insensitive, e.g. `/Internal shared storage/DCIM`) or by 1-based
//! index (`/1/DCIM`). An empty path (`""`, `"/"`) denotes the *device root*
//! whose listing is the set of storages.

/// A parsed device path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevPath {
    /// First raw segment (storage reference). Empty for the device root.
    pub storage_ref: String,
    /// Remaining segments (directories, then optional file name).
    pub segments: Vec<String>,
}

impl DevPath {
    /// Parses a user-supplied device path.
    ///
    /// # Errors
    /// Returns [`crate::error::Error::InvalidArgument`] when a segment is
    /// empty (double slashes) or the path contains `..`.
    pub fn parse(input: &str) -> crate::error::Result<Self> {
        let trimmed = input.trim();
        let mut parts: Vec<&str> = trimmed.split('/').collect();
        // A single leading or trailing slash marks an absolute path; any
        // other empty segment (double slash in the middle) is an error.
        if parts.first().is_some_and(|p| p.is_empty()) {
            parts.remove(0);
        }
        if parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }

        for p in &parts {
            // Both dot forms are rejected rather than normalized: collapsing
            // them would change what the user asked for, and passing `.`
            // through to the phone asks it to resolve a segment we should own.
            if *p == ".." || *p == "." {
                return Err(crate::error::Error::InvalidArgument(format!(
                    "`{p}` is not allowed in device paths: {input}"
                )));
            }
            if p.trim().is_empty() {
                return Err(crate::error::Error::InvalidArgument(format!(
                    "empty path segment in {input}"
                )));
            }
            // A NUL cannot be represented in an MTP name at all, and control
            // characters have no legitimate use in one.
            if p.chars().any(|c| c.is_control()) {
                return Err(crate::error::Error::InvalidArgument(format!(
                    "control character in path segment `{p}` of {input}"
                )));
            }
        }

        let mut it = parts.into_iter();
        let storage_ref = it.next().unwrap_or_default().to_string();
        let segments = it.map(str::to_string).collect();

        Ok(Self {
            storage_ref,
            segments,
        })
    }

    /// True when the path addresses the device root (the storage list).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.storage_ref.is_empty()
    }

    /// Rebuilds a displayable path string.
    #[must_use]
    pub fn display(&self) -> String {
        if self.is_root() {
            return "/".to_string();
        }
        let mut s = String::from("/");
        s.push_str(&self.storage_ref);
        for seg in &self.segments {
            s.push('/');
            s.push_str(seg);
        }
        s
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn parses_storage_and_segments() {
        let p = DevPath::parse("/Internal shared storage/DCIM/Camera").expect("parse failed");
        assert_eq!(p.storage_ref, "Internal shared storage");
        assert_eq!(p.segments, vec!["DCIM".to_string(), "Camera".to_string()]);
        assert!(!p.is_root());
        assert_eq!(p.display(), "/Internal shared storage/DCIM/Camera");
    }

    #[test]
    fn root_variants_are_recognized() {
        for raw in ["", "/", "  /  "] {
            let p = DevPath::parse(raw).expect("parse failed");
            assert!(p.is_root());
            assert_eq!(p.display(), "/");
        }
    }

    #[test]
    fn rejects_dotdot() {
        assert!(matches!(
            DevPath::parse("/a/../b"),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn rejects_dot_segment() {
        // `.` used to be passed through to the phone to resolve.
        for raw in ["/a/./b", "/./a", "/a/."] {
            assert!(
                matches!(DevPath::parse(raw), Err(Error::InvalidArgument(_))),
                "`{raw}` must be rejected"
            );
        }
    }

    #[test]
    fn rejects_control_characters() {
        for raw in ["/a/\u{0}b", "/a/line\nbreak"] {
            assert!(
                matches!(DevPath::parse(raw), Err(Error::InvalidArgument(_))),
                "`{raw}` must be rejected"
            );
        }
    }

    #[test]
    fn rejects_double_slash_segment() {
        assert!(matches!(
            DevPath::parse("//x"),
            Err(Error::InvalidArgument(_))
        ));
    }
}
