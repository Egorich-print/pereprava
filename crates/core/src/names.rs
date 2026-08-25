//! Name-matching helpers tolerant to Unicode normalization differences.
//!
//! Phones report names in NFC, macOS LaunchServices/Finder compose user
//! paths in NFD ("й" = "и" + U+0306). Raw byte comparison breaks lookups
//! for any non-ASCII name, so all name matching goes through [`names_eq`].

use unicode_normalization::UnicodeNormalization;

/// Unicode-normalization-insensitive equality (both sides folded to NFC).
#[must_use]
pub fn names_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    a.nfc().eq(b.nfc())
}

/// Case- and normalization-insensitive equality.
#[must_use]
pub fn names_eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || {
        let lower = |s: &str| s.to_lowercase();
        names_eq(&lower(a), &lower(b))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn nfd_matches_nfc() {
        let word = "Внутренний общий накопитель";
        let decomp: String = word.nfd().collect();
        assert_ne!(word, decomp, "fixture must actually be decomposed");
        assert!(names_eq(word, &decomp));
        assert!(names_eq_ci(&word.to_uppercase(), &decomp));
        assert!(!names_eq("foo", "bar"));
    }

    #[test]
    fn ascii_fast_path_unchanged() {
        assert!(names_eq("DCIM", "DCIM"));
        assert!(names_eq_ci("dcim", "DCIM"));
    }
}
