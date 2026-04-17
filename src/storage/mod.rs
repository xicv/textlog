//! Storage subsystem: SQLite ring buffer + daily Markdown archive.
//!
//! Public types used by both the `markdown` writer and the `sqlite`
//! ring-buffer (Phase 6). The MD archive is the durable, never-trimmed
//! record; SQLite is a bounded query index over recent captures.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod markdown;
pub mod sqlite;

pub use sqlite::Storage;

/// Capture kind discriminator. Mirrors the spec's `captures.kind` column
/// and the `kind:` frontmatter field on the daily Markdown file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Image,
    File,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Image => "image",
            Kind::File => "file",
        }
    }
}

/// Lowercase 64-char hex of a 32-byte SHA-256 digest.
pub(crate) fn hex_lower(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Parse a 64-char hex string back into 32 bytes. Accepts upper- or
/// lowercase digits.
pub(crate) fn parse_hex(s: &str) -> crate::error::Result<[u8; 32]> {
    use crate::error::Error;
    if s.len() != 64 {
        return Err(Error::Storage(format!(
            "sha256 hex must be 64 chars, got {}",
            s.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(s.as_bytes()[i * 2])?;
        let lo = hex_nibble(s.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> crate::error::Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(crate::error::Error::Storage(format!(
            "non-hex char {:?} in sha256",
            c as char
        ))),
    }
}

/// One row of a `Storage::search` result. `duplicate_of` is `Some(id)` if
/// another row in the *same* result set carries the same `sha256` and was
/// chosen as canonical (first occurrence in the ts-DESC ordering). The
/// MCP layer uses this to elide redundant content from Claude's view.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub row: CaptureRow,
    pub duplicate_of: Option<i64>,
}

/// One row of the captures table — also the unit appended to the daily
/// Markdown file. `id` is `0` until the row has been inserted into SQLite.
#[derive(Debug, Clone)]
pub struct CaptureRow {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub kind: Kind,
    pub sha256: [u8; 32],
    pub size_bytes: usize,
    /// For `Text`: the clipboard contents.
    /// For `Image`: the OCR'd text (single source of truth, no separate
    /// `ocr_text` field per spec §Data Format).
    pub content: Option<String>,
    /// Mean Apple Vision confidence, images only.
    pub ocr_confidence: Option<f32>,
    pub source_app: Option<String>,
    pub source_url: Option<String>,
    /// Daily MD file the row is mirrored into.
    pub md_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_serializes_lowercase() {
        assert_eq!(Kind::Text.as_str(), "text");
        assert_eq!(Kind::Image.as_str(), "image");
        assert_eq!(Kind::File.as_str(), "file");
    }

    #[test]
    fn kind_serde_roundtrips_lowercase() {
        let json = serde_json::to_string(&Kind::Image).unwrap();
        assert_eq!(json, "\"image\"");
        let back: Kind = serde_json::from_str("\"file\"").unwrap();
        assert_eq!(back, Kind::File);
    }

    #[test]
    fn hex_lower_is_64_chars_lowercase() {
        let h = hex_lower(&[0xAB; 32]);
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(h, "ab".repeat(32));
    }

    #[test]
    fn parse_hex_roundtrips() {
        let original = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]
            .into_iter()
            .cycle()
            .take(32)
            .collect::<Vec<_>>();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&original);
        let hex = hex_lower(&arr);
        let back = parse_hex(&hex).unwrap();
        assert_eq!(arr, back);
    }

    #[test]
    fn parse_hex_accepts_uppercase() {
        let upper = "AB".repeat(32);
        let bytes = parse_hex(&upper).expect("uppercase hex should parse");
        assert_eq!(bytes, [0xab; 32]);
    }

    #[test]
    fn parse_hex_rejects_wrong_length() {
        let err = parse_hex("deadbeef").unwrap_err();
        assert!(format!("{err}").contains("64 chars"));
    }

    #[test]
    fn parse_hex_rejects_non_hex_char() {
        let bad = "g".to_string() + &"a".repeat(63);
        let err = parse_hex(&bad).unwrap_err();
        assert!(format!("{err}").contains("non-hex"));
    }
}
