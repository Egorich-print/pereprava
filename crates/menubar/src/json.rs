//! Minimal JSON field extraction for the status file.
//!
//! The daemon writes a flat object with known keys; a full parser is
//! unnecessary — we only ever need string and integer fields at top level.

/// Parsed view of the daemon's status file.
#[derive(Debug, Default, Clone)]
pub struct Status {
    /// `waiting` | `attached` | `gone`
    pub state: String,
    /// Device model name (e.g. "A065").
    pub model: String,
    /// Mount point actually used, empty when not mounted.
    pub mounted: String,
    /// Total bytes pulled from the phone.
    pub rx: u64,
    /// Total bytes pushed to the phone.
    pub tx: u64,
    /// Instantaneous pull rate (bytes/sec), computed by the daemon.
    pub speed_rx: u64,
    /// Instantaneous push rate (bytes/sec).
    pub speed_tx: u64,
}

impl Status {
    /// Parses the known fields out of the flat JSON object.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        Self {
            state: jstr(raw, "state").unwrap_or_default(),
            model: jstr(raw, "model").unwrap_or_default(),
            mounted: jstr(raw, "mounted").unwrap_or_default(),
            rx: jnum(raw, "rx").unwrap_or(0),
            tx: jnum(raw, "tx").unwrap_or(0),
            speed_rx: jnum(raw, "speed_rx").unwrap_or(0),
            speed_tx: jnum(raw, "speed_tx").unwrap_or(0),
        }
    }
}

/// Extracts `"key":"value"` (top level, no nested objects in our schema).
#[must_use]
pub fn jstr(src: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let i = src.find(&pat)? + pat.len();
    let rest = &src[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extracts `"key":123`.
#[must_use]
pub fn jnum(src: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let i = src.find(&pat)? + pat.len();
    let rest = &src[i..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}
