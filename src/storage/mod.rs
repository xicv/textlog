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
}
