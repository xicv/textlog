//! Daily Markdown archive writer.
//!
//! Each capture is appended as a YAML-frontmatter block followed by the
//! body. The MD file is the durable record (never trimmed); SQLite is the
//! query index. Format mirrors spec §Data Format → "Markdown daily file".

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::storage::{hex_lower, CaptureRow};

/// Render a capture row into the v2.0 frontmatter+body Markdown block.
/// Always ends with a trailing newline so consecutive appends produce a
/// well-formed multi-document stream.
pub fn render(row: &CaptureRow) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("---\n");
    let _ = writeln!(out, "timestamp: {}", row.ts.to_rfc3339());
    let _ = writeln!(out, "kind: {}", row.kind.as_str());
    let _ = writeln!(out, "sha256: {}", hex_lower(&row.sha256));
    let _ = writeln!(out, "size_bytes: {}", row.size_bytes);
    if let Some(conf) = row.ocr_confidence {
        let _ = writeln!(out, "ocr_confidence: {conf}");
    }
    if let Some(app) = &row.source_app {
        let _ = writeln!(out, "source_app: \"{app}\"");
    }
    if let Some(url) = &row.source_url {
        let _ = writeln!(out, "source_url: \"{url}\"");
    }
    out.push_str("---\n");

    match &row.content {
        Some(body) => {
            out.push_str(body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        }
        None => out.push('\n'),
    }
    out
}

/// Append a rendered row to the daily file, creating the parent
/// directory on demand.
pub fn append(path: &Path, row: &CaptureRow) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(render(row).as_bytes())?;
    Ok(())
}

/// Resolve the daily Markdown file path: `<log_dir>/<ts.format(date_format)>.md`.
/// Expands a leading `~/` against the user's home directory.
pub fn daily_path(log_dir: &str, date_format: &str, ts: DateTime<Utc>) -> PathBuf {
    let mut base = super::expand_tilde(log_dir);
    let date = ts.format(date_format).to_string();
    base.push(format!("{date}.md"));
    base
}


#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn sample_text_row(content: &str) -> CaptureRow {
        CaptureRow {
            id: 0,
            ts: Utc.with_ymd_and_hms(2026, 4, 17, 12, 34, 56).unwrap(),
            kind: crate::storage::Kind::Text,
            sha256: [0xab; 32],
            size_bytes: content.len(),
            content: Some(content.to_string()),
            ocr_confidence: None,
            source_app: None,
            source_url: None,
            md_path: PathBuf::from("/tmp/textlog/2026-04-17.md"),
        }
    }

    fn sample_image_row(ocr: &str, confidence: f32) -> CaptureRow {
        CaptureRow {
            id: 0,
            ts: Utc.with_ymd_and_hms(2026, 4, 17, 12, 34, 56).unwrap(),
            kind: crate::storage::Kind::Image,
            sha256: [0x9f; 32],
            size_bytes: 82_431,
            content: Some(ocr.to_string()),
            ocr_confidence: Some(confidence),
            source_app: Some("Safari".into()),
            source_url: None,
            md_path: PathBuf::from("/tmp/textlog/2026-04-17.md"),
        }
    }

    #[test]
    fn render_text_includes_required_frontmatter_fields() {
        let row = sample_text_row("hello world");
        let md = render(&row);
        assert!(md.contains("timestamp: 2026-04-17T12:34:56+00:00"));
        assert!(md.contains("kind: text"));
        assert!(md.contains("size_bytes: 11"));
        assert!(md.contains(&format!("sha256: {}", "ab".repeat(32))));
    }

    #[test]
    fn render_text_body_is_content() {
        let row = sample_text_row("error: no space left on device");
        let md = render(&row);
        assert!(
            md.contains("\nerror: no space left on device\n"),
            "body must appear after the second `---` delimiter; got:\n{md}"
        );
    }

    #[test]
    fn render_starts_and_ends_with_delimiters() {
        let md = render(&sample_text_row("body"));
        assert!(md.starts_with("---\n"), "render must start with `---`\n{md}");
        assert!(md.ends_with('\n'), "render must end with a newline\n{md}");
        // Exactly two delimiter lines (open + close of frontmatter), the
        // body is *not* wrapped in a trailing `---`.
        assert_eq!(md.matches("\n---\n").count(), 1, "exactly one closing delim");
    }

    #[test]
    fn render_image_includes_ocr_confidence() {
        let md = render(&sample_image_row("captured text", 0.93));
        assert!(md.contains("kind: image"));
        assert!(md.contains("ocr_confidence: 0.93"));
        assert!(md.contains("source_app: \"Safari\""));
    }

    #[test]
    fn render_image_body_is_ocr_text() {
        let md = render(&sample_image_row("OCR'd line", 0.5));
        assert!(md.contains("\nOCR'd line\n"), "image body should be OCR text:\n{md}");
    }

    #[test]
    fn render_omits_optional_fields_when_none() {
        let md = render(&sample_text_row("plain"));
        assert!(!md.contains("source_app:"), "no source_app line when None");
        assert!(!md.contains("source_url:"), "no source_url line when None");
        assert!(!md.contains("ocr_confidence:"), "no ocr_confidence line for text");
    }

    #[test]
    fn render_includes_source_url_when_some() {
        let mut row = sample_text_row("see this");
        row.source_url = Some("https://example.com/x".into());
        let md = render(&row);
        assert!(md.contains("source_url: \"https://example.com/x\""));
    }

    #[test]
    fn render_sha256_is_lowercase_hex_64_chars() {
        let row = sample_text_row("x");
        let md = render(&row);
        let line = md
            .lines()
            .find(|l| l.starts_with("sha256: "))
            .expect("sha256 line present");
        let hex = line.trim_start_matches("sha256: ");
        assert_eq!(hex.len(), 64, "sha256 hex must be 64 chars, got {}", hex.len());
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn daily_path_uses_date_format() {
        let ts = Utc.with_ymd_and_hms(2026, 4, 17, 0, 0, 0).unwrap();
        let p = daily_path("/var/log/textlog", "%Y-%m-%d", ts);
        assert_eq!(p, PathBuf::from("/var/log/textlog/2026-04-17.md"));
    }

    #[test]
    fn daily_path_supports_custom_date_format() {
        let ts = Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap();
        let p = daily_path("/logs", "%Y/%m/%d", ts);
        assert_eq!(p, PathBuf::from("/logs/2026/01/05.md"));
    }

    #[test]
    fn daily_path_expands_leading_tilde() {
        let ts = Utc.with_ymd_and_hms(2026, 4, 17, 0, 0, 0).unwrap();
        let p = daily_path("~/textlog/logs", "%Y-%m-%d", ts);
        let s = p.to_string_lossy();
        assert!(!s.starts_with("~/"), "tilde must be expanded, got `{s}`");
        assert!(s.ends_with("/textlog/logs/2026-04-17.md"), "got `{s}`");
    }

    #[test]
    fn append_creates_parent_directory_and_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/dir/2026-04-17.md");
        let row = sample_text_row("first entry");

        append(&path, &row).expect("first append must succeed");

        let body = std::fs::read_to_string(&path).expect("file should exist");
        assert!(body.contains("first entry"));
        assert!(body.contains("kind: text"));
    }

    #[test]
    fn append_appends_without_truncating() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2026-04-17.md");

        append(&path, &sample_text_row("alpha")).unwrap();
        append(&path, &sample_text_row("bravo")).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("alpha"));
        assert!(body.contains("bravo"));
        // Two frontmatter blocks → two opening delimiters at the start
        // of a line (the very first one starts at offset 0; the second
        // appears after a newline).
        let opening_delims =
            usize::from(body.starts_with("---\n")) + body.matches("\n---\n").count();
        assert!(
            opening_delims >= 2,
            "expected at least 2 frontmatter blocks, got {opening_delims}\n{body}"
        );
    }
}
