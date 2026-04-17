//! SQLite ring buffer + FTS5 query index over recent captures.
//!
//! The MD archive (see `storage::markdown`) is the durable log;
//! SQLite is a bounded query index. `Storage::insert` writes to both,
//! then trims SQLite to `ring_buffer_size` rows. The MD file is never
//! trimmed.
//!
//! Threading: holds an `Arc<Mutex<Connection>>` so the type is `Send +
//! Sync` and can be shared across the MCP server's async tasks via
//! `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::storage::{markdown, CaptureRow, Kind, SearchHit};

pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    ring_buffer_size: usize,
}

impl Storage {
    /// Open or create the SQLite file at `path`, applying the v2.0 schema.
    pub fn open(path: impl AsRef<Path>, ring_buffer_size: usize) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::with_connection(conn, ring_buffer_size)
    }

    /// Open an in-memory database — primarily for tests.
    pub fn open_in_memory(ring_buffer_size: usize) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::with_connection(conn, ring_buffer_size)
    }

    fn with_connection(conn: Connection, ring_buffer_size: usize) -> Result<Self> {
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            ring_buffer_size,
        })
    }

    /// Insert a capture row, append to its daily MD file, and trim
    /// SQLite to `ring_buffer_size` rows. Returns the new row id.
    pub fn insert(&self, row: &CaptureRow) -> Result<i64> {
        let id = {
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO captures
                    (ts, kind, sha256, size_bytes, content,
                     ocr_confidence, source_app, source_url, md_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    row.ts.to_rfc3339(),
                    row.kind.as_str(),
                    super::hex_lower(&row.sha256),
                    row.size_bytes as i64,
                    row.content,
                    row.ocr_confidence,
                    row.source_app,
                    row.source_url,
                    row.md_path.to_string_lossy().into_owned(),
                ],
            )?;
            let id = conn.last_insert_rowid();
            trim_ring_buffer(&conn, self.ring_buffer_size)?;
            id
        };

        markdown::append(&row.md_path, row)?;
        Ok(id)
    }

    /// Most recent N captures, deduplicated by sha256 (newest of each
    /// hash kept). Optional kind filter.
    pub fn get_recent(&self, n: u32, kind: Option<Kind>) -> Result<Vec<CaptureRow>> {
        let conn = self.lock()?;
        let kind_str = kind.map(|k| k.as_str().to_string());

        // SQLite parameter trick: NULL bound to a kind filter means "any".
        let mut stmt = conn.prepare(
            "SELECT id, ts, kind, sha256, size_bytes, content,
                    ocr_confidence, source_app, source_url, md_path
             FROM captures
             WHERE id IN (
                 SELECT MAX(id) FROM captures
                 WHERE (?1 IS NULL OR kind = ?1)
                 GROUP BY sha256
             )
             ORDER BY ts DESC
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(rusqlite::params![kind_str, n as i64], row_from_sqlite)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // query_map yields Result<Result<CaptureRow>>; flatten the inner.
        rows.into_iter().collect()
    }

    /// Most recent image capture, or None.
    pub fn get_latest_image(&self) -> Result<Option<CaptureRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, ts, kind, sha256, size_bytes, content,
                    ocr_confidence, source_app, source_url, md_path
             FROM captures
             WHERE kind = 'image'
             ORDER BY ts DESC
             LIMIT 1",
        )?;
        let mut iter = stmt.query_map([], row_from_sqlite)?;
        match iter.next() {
            None => Ok(None),
            Some(Ok(Ok(row))) => Ok(Some(row)),
            Some(Ok(Err(e))) => Err(e),
            Some(Err(e)) => Err(e.into()),
        }
    }

    /// Full-text search via FTS5. Returns up to `limit` rows matching
    /// `query` (FTS5 syntax), optionally bounded to `ts >= since`.
    /// Each hit carries a `duplicate_of` marker pointing at the
    /// canonical (first occurrence in ts-DESC order) row sharing its
    /// sha256 within this result set.
    pub fn search(
        &self,
        query: &str,
        limit: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.lock()?;
        let since_str = since.map(|t| t.to_rfc3339());

        let mut stmt = conn.prepare(
            "SELECT c.id, c.ts, c.kind, c.sha256, c.size_bytes, c.content,
                    c.ocr_confidence, c.source_app, c.source_url, c.md_path
             FROM captures c
             JOIN captures_fts f ON c.id = f.rowid
             WHERE captures_fts MATCH ?1
               AND (?2 IS NULL OR c.ts >= ?2)
             ORDER BY c.ts DESC
             LIMIT ?3",
        )?;

        let rows = stmt
            .query_map(
                rusqlite::params![query, since_str, limit as i64],
                row_from_sqlite,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let rows = rows.into_iter().collect::<Result<Vec<CaptureRow>>>()?;

        // duplicate_of: first occurrence (in ts-DESC order) per sha256 is
        // canonical; later occurrences point back to that id.
        let mut canonical: std::collections::HashMap<[u8; 32], i64> =
            std::collections::HashMap::new();
        let hits = rows
            .into_iter()
            .map(|row| {
                let dup_of = canonical.get(&row.sha256).copied();
                if dup_of.is_none() {
                    canonical.insert(row.sha256, row.id);
                }
                SearchHit { row, duplicate_of: dup_of }
            })
            .collect();

        Ok(hits)
    }

    /// Delete every row whose `ts >= ts`. MD files are *not* touched
    /// (per spec — user can delete those manually).
    pub fn clear_since(&self, ts: DateTime<Utc>) -> Result<usize> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM captures WHERE ts >= ?1",
            rusqlite::params![ts.to_rfc3339()],
        )?;
        Ok(n)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| Error::Storage(format!("connection mutex poisoned: {e}")))
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS captures (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            ts              TEXT    NOT NULL,
            kind            TEXT    NOT NULL,
            sha256          TEXT    NOT NULL,
            size_bytes      INTEGER NOT NULL,
            content         TEXT,
            ocr_confidence  REAL,
            source_app      TEXT,
            source_url      TEXT,
            md_path         TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_captures_ts     ON captures(ts DESC);
         CREATE INDEX IF NOT EXISTS idx_captures_sha256 ON captures(sha256);
         CREATE INDEX IF NOT EXISTS idx_captures_kind   ON captures(kind);

         CREATE VIRTUAL TABLE IF NOT EXISTS captures_fts USING fts5(
             content,
             content='captures',
             content_rowid='id'
         );

         CREATE TRIGGER IF NOT EXISTS captures_ai
         AFTER INSERT ON captures BEGIN
             INSERT INTO captures_fts(rowid, content) VALUES (new.id, new.content);
         END;

         CREATE TRIGGER IF NOT EXISTS captures_ad
         AFTER DELETE ON captures BEGIN
             INSERT INTO captures_fts(captures_fts, rowid, content)
             VALUES ('delete', old.id, old.content);
         END;

         CREATE TRIGGER IF NOT EXISTS captures_au
         AFTER UPDATE ON captures BEGIN
             INSERT INTO captures_fts(captures_fts, rowid, content)
             VALUES ('delete', old.id, old.content);
             INSERT INTO captures_fts(rowid, content) VALUES (new.id, new.content);
         END;",
    )?;
    Ok(())
}

fn trim_ring_buffer(conn: &Connection, size: usize) -> Result<()> {
    if size == 0 {
        // 0 disables trimming entirely.
        return Ok(());
    }
    conn.execute(
        "DELETE FROM captures
         WHERE id NOT IN (
             SELECT id FROM captures ORDER BY id DESC LIMIT ?1
         )",
        rusqlite::params![size as i64],
    )?;
    Ok(())
}

/// Map a SQLite row → CaptureRow. Returns the inner Result so callers
/// can flatten through `.collect::<Result<Vec<_>>>()`.
#[allow(clippy::type_complexity)]
fn row_from_sqlite(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<CaptureRow>> {
    let id: i64 = row.get(0)?;
    let ts_str: String = row.get(1)?;
    let kind_str: String = row.get(2)?;
    let sha_hex: String = row.get(3)?;
    let size_bytes: i64 = row.get(4)?;
    let content: Option<String> = row.get(5)?;
    let ocr_confidence: Option<f32> = row.get(6)?;
    let source_app: Option<String> = row.get(7)?;
    let source_url: Option<String> = row.get(8)?;
    let md_path: String = row.get(9)?;

    Ok(decode_row(
        id,
        ts_str,
        kind_str,
        sha_hex,
        size_bytes,
        content,
        ocr_confidence,
        source_app,
        source_url,
        md_path,
    ))
}

#[allow(clippy::too_many_arguments)]
fn decode_row(
    id: i64,
    ts_str: String,
    kind_str: String,
    sha_hex: String,
    size_bytes: i64,
    content: Option<String>,
    ocr_confidence: Option<f32>,
    source_app: Option<String>,
    source_url: Option<String>,
    md_path: String,
) -> Result<CaptureRow> {
    let ts = DateTime::parse_from_rfc3339(&ts_str)
        .map_err(|e| Error::Storage(format!("bad ts {ts_str:?}: {e}")))?
        .with_timezone(&Utc);
    let kind = match kind_str.as_str() {
        "text" => Kind::Text,
        "image" => Kind::Image,
        "file" => Kind::File,
        other => return Err(Error::Storage(format!("unknown kind {other:?}"))),
    };
    let sha256 = super::parse_hex(&sha_hex)?;
    Ok(CaptureRow {
        id,
        ts,
        kind,
        sha256,
        size_bytes: size_bytes as usize,
        content,
        ocr_confidence,
        source_app,
        source_url,
        md_path: md_path.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn row(
        ts: DateTime<Utc>,
        kind: Kind,
        sha: u8,
        content: &str,
        md_dir: &Path,
    ) -> CaptureRow {
        CaptureRow {
            id: 0,
            ts,
            kind,
            sha256: [sha; 32],
            size_bytes: content.len(),
            content: Some(content.to_string()),
            ocr_confidence: if matches!(kind, Kind::Image) {
                Some(0.9)
            } else {
                None
            },
            source_app: None,
            source_url: None,
            md_path: md_dir.join("2026-04-17.md"),
        }
    }

    #[test]
    fn open_in_memory_creates_schema() {
        let s = Storage::open_in_memory(100).expect("open_in_memory");
        // Should be able to query an empty table without error.
        let recent = s.get_recent(10, None).expect("get_recent on empty db");
        assert!(recent.is_empty());
    }

    #[test]
    fn open_creates_db_file_on_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.db");
        let _s = Storage::open(&path, 100).unwrap();
        assert!(path.exists(), "db file should be created on first open");
    }

    #[test]
    fn open_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.db");
        {
            let s = Storage::open(&path, 100).unwrap();
            s.insert(&row(Utc::now(), Kind::Text, 1, "first", tmp.path()))
                .unwrap();
        }
        // Reopen — must not blow up, must keep the row.
        let s = Storage::open(&path, 100).unwrap();
        let recent = s.get_recent(10, None).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].content.as_deref(), Some("first"));
    }

    #[test]
    fn insert_returns_incrementing_row_ids() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let id1 = s.insert(&row(Utc::now(), Kind::Text, 1, "a", tmp.path())).unwrap();
        let id2 = s.insert(&row(Utc::now(), Kind::Text, 2, "b", tmp.path())).unwrap();
        assert!(id2 > id1, "id2 ({id2}) must be greater than id1 ({id1})");
    }

    #[test]
    fn insert_writes_md_file() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let r = row(Utc::now(), Kind::Text, 1, "log line", tmp.path());
        s.insert(&r).unwrap();
        let body = std::fs::read_to_string(&r.md_path).unwrap();
        assert!(body.contains("log line"));
        assert!(body.contains("kind: text"));
    }

    #[test]
    fn insert_appends_to_existing_md() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let now = Utc::now();
        let r1 = row(now, Kind::Text, 1, "alpha", tmp.path());
        let r2 = row(now, Kind::Text, 2, "bravo", tmp.path());
        s.insert(&r1).unwrap();
        s.insert(&r2).unwrap();
        let body = std::fs::read_to_string(&r1.md_path).unwrap();
        assert!(body.contains("alpha"));
        assert!(body.contains("bravo"));
    }

    #[test]
    fn ring_buffer_trims_to_size() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(2).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        for i in 0..5 {
            s.insert(&row(
                base + chrono::Duration::seconds(i as i64),
                Kind::Text,
                i as u8 + 1,
                &format!("entry {i}"),
                tmp.path(),
            ))
            .unwrap();
        }
        let all = s.get_recent(100, None).unwrap();
        assert_eq!(all.len(), 2, "ring buffer of size 2 keeps last 2");
        assert_eq!(all[0].content.as_deref(), Some("entry 4"));
        assert_eq!(all[1].content.as_deref(), Some("entry 3"));
    }

    #[test]
    fn ring_buffer_size_zero_disables_trimming() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(0).unwrap();
        for i in 0..3 {
            s.insert(&row(
                Utc::now() + chrono::Duration::seconds(i as i64),
                Kind::Text,
                i as u8 + 1,
                &format!("e{i}"),
                tmp.path(),
            ))
            .unwrap();
        }
        let all = s.get_recent(100, None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn get_recent_orders_by_ts_descending() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        s.insert(&row(base, Kind::Text, 1, "old", tmp.path())).unwrap();
        s.insert(&row(base + chrono::Duration::seconds(2), Kind::Text, 2, "new", tmp.path()))
            .unwrap();
        s.insert(&row(base + chrono::Duration::seconds(1), Kind::Text, 3, "mid", tmp.path()))
            .unwrap();
        let r = s.get_recent(10, None).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].content.as_deref(), Some("new"));
        assert_eq!(r[1].content.as_deref(), Some("mid"));
        assert_eq!(r[2].content.as_deref(), Some("old"));
    }

    #[test]
    fn get_recent_caps_by_n() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        for i in 0..5 {
            s.insert(&row(
                Utc::now() + chrono::Duration::seconds(i as i64),
                Kind::Text,
                i as u8 + 1,
                &format!("e{i}"),
                tmp.path(),
            ))
            .unwrap();
        }
        let r = s.get_recent(2, None).unwrap();
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn get_recent_filters_by_kind() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let now = Utc::now();
        s.insert(&row(now, Kind::Text, 1, "t1", tmp.path())).unwrap();
        s.insert(&row(now + chrono::Duration::seconds(1), Kind::Image, 2, "i1", tmp.path()))
            .unwrap();
        s.insert(&row(now + chrono::Duration::seconds(2), Kind::Text, 3, "t2", tmp.path()))
            .unwrap();
        let images = s.get_recent(10, Some(Kind::Image)).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].kind, Kind::Image);
        assert_eq!(images[0].content.as_deref(), Some("i1"));
    }

    #[test]
    fn get_recent_dedupes_by_sha256() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        // Three rows, same sha256 → one returned (the newest).
        let now = Utc::now();
        s.insert(&row(now, Kind::Text, 7, "first", tmp.path())).unwrap();
        s.insert(&row(now + chrono::Duration::seconds(1), Kind::Text, 7, "second", tmp.path()))
            .unwrap();
        s.insert(&row(now + chrono::Duration::seconds(2), Kind::Text, 7, "third", tmp.path()))
            .unwrap();
        let r = s.get_recent(10, None).unwrap();
        assert_eq!(r.len(), 1, "deduped by sha256");
        // Newest (highest id) wins.
        assert_eq!(r[0].content.as_deref(), Some("third"));
    }

    #[test]
    fn get_latest_image_returns_most_recent_image() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let now = Utc::now();
        s.insert(&row(now, Kind::Text, 1, "txt", tmp.path())).unwrap();
        s.insert(&row(now + chrono::Duration::seconds(1), Kind::Image, 2, "old img", tmp.path()))
            .unwrap();
        s.insert(&row(now + chrono::Duration::seconds(2), Kind::Text, 3, "more txt", tmp.path()))
            .unwrap();
        s.insert(&row(now + chrono::Duration::seconds(3), Kind::Image, 4, "new img", tmp.path()))
            .unwrap();
        let img = s.get_latest_image().unwrap().expect("an image exists");
        assert_eq!(img.content.as_deref(), Some("new img"));
    }

    #[test]
    fn get_latest_image_returns_none_when_no_images() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        s.insert(&row(Utc::now(), Kind::Text, 1, "only text", tmp.path())).unwrap();
        assert!(s.get_latest_image().unwrap().is_none());
    }

    #[test]
    fn clear_since_deletes_rows_at_or_after_ts() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        s.insert(&row(base, Kind::Text, 1, "before", tmp.path())).unwrap();
        s.insert(&row(base + chrono::Duration::seconds(60), Kind::Text, 2, "boundary", tmp.path()))
            .unwrap();
        s.insert(&row(base + chrono::Duration::seconds(120), Kind::Text, 3, "after", tmp.path()))
            .unwrap();

        let cutoff = base + chrono::Duration::seconds(60);
        let deleted = s.clear_since(cutoff).unwrap();
        assert_eq!(deleted, 2, "boundary + after row deleted");

        let rest = s.get_recent(10, None).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].content.as_deref(), Some("before"));
    }

    #[test]
    fn round_trip_preserves_all_optional_fields() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let mut r = row(
            Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap(),
            Kind::Image,
            42,
            "OCR text",
            tmp.path(),
        );
        r.source_app = Some("Safari".into());
        r.source_url = Some("https://example.com/x".into());
        r.ocr_confidence = Some(0.87);
        s.insert(&r).unwrap();

        let got = s.get_recent(10, None).unwrap();
        assert_eq!(got.len(), 1);
        let g = &got[0];
        assert_eq!(g.kind, Kind::Image);
        assert_eq!(g.sha256, [42u8; 32]);
        assert_eq!(g.source_app.as_deref(), Some("Safari"));
        assert_eq!(g.source_url.as_deref(), Some("https://example.com/x"));
        assert!((g.ocr_confidence.unwrap() - 0.87).abs() < 1e-5);
        assert_eq!(g.md_path, r.md_path);
    }

    #[test]
    fn search_finds_word_match() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        s.insert(&row(Utc::now(), Kind::Text, 1, "hello world", tmp.path())).unwrap();
        s.insert(&row(Utc::now(), Kind::Text, 2, "unrelated content", tmp.path())).unwrap();

        let hits = s.search("hello", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.content.as_deref(), Some("hello world"));
        assert_eq!(hits[0].duplicate_of, None);
    }

    #[test]
    fn search_returns_empty_for_no_match() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        s.insert(&row(Utc::now(), Kind::Text, 1, "alpha bravo", tmp.path())).unwrap();
        let hits = s.search("zulu", 10, None).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_caps_by_limit() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        for i in 0..5 {
            s.insert(&row(
                Utc::now() + chrono::Duration::seconds(i as i64),
                Kind::Text,
                i as u8 + 1,
                "needle in the stack",
                tmp.path(),
            ))
            .unwrap();
        }
        let hits = s.search("needle", 2, None).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_filters_by_since_timestamp() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        s.insert(&row(base, Kind::Text, 1, "needle one", tmp.path())).unwrap();
        s.insert(&row(base + chrono::Duration::seconds(60), Kind::Text, 2, "needle two", tmp.path()))
            .unwrap();
        s.insert(&row(base + chrono::Duration::seconds(120), Kind::Text, 3, "needle three", tmp.path()))
            .unwrap();

        let cutoff = base + chrono::Duration::seconds(60);
        let hits = s.search("needle", 10, Some(cutoff)).unwrap();
        // Only rows at-or-after cutoff: "needle two" and "needle three".
        assert_eq!(hits.len(), 2);
        let texts: Vec<&str> = hits
            .iter()
            .filter_map(|h| h.row.content.as_deref())
            .collect();
        assert!(texts.contains(&"needle two"));
        assert!(texts.contains(&"needle three"));
        assert!(!texts.contains(&"needle one"));
    }

    #[test]
    fn search_orders_by_ts_descending() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        s.insert(&row(base, Kind::Text, 1, "needle old", tmp.path())).unwrap();
        s.insert(&row(base + chrono::Duration::seconds(60), Kind::Text, 2, "needle new", tmp.path()))
            .unwrap();
        let hits = s.search("needle", 10, None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].row.content.as_deref(), Some("needle new"));
        assert_eq!(hits[1].row.content.as_deref(), Some("needle old"));
    }

    #[test]
    fn search_marks_duplicate_of_within_result_set() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        // Three rows, same sha256 (and identical content so they all match).
        let id_old = s
            .insert(&row(base, Kind::Text, 7, "needle dup", tmp.path()))
            .unwrap();
        let id_mid = s
            .insert(&row(
                base + chrono::Duration::seconds(60),
                Kind::Text,
                7,
                "needle dup",
                tmp.path(),
            ))
            .unwrap();
        let id_new = s
            .insert(&row(
                base + chrono::Duration::seconds(120),
                Kind::Text,
                7,
                "needle dup",
                tmp.path(),
            ))
            .unwrap();

        let hits = s.search("needle", 10, None).unwrap();
        assert_eq!(hits.len(), 3, "search returns ALL matches (no dedup)");
        // Result is ts DESC: [id_new, id_mid, id_old]
        assert_eq!(hits[0].row.id, id_new);
        assert_eq!(hits[0].duplicate_of, None, "first occurrence is canonical");
        assert_eq!(hits[1].row.id, id_mid);
        assert_eq!(hits[1].duplicate_of, Some(id_new));
        assert_eq!(hits[2].row.id, id_old);
        assert_eq!(hits[2].duplicate_of, Some(id_new));
    }

    #[test]
    fn search_handles_empty_content_rows() {
        let tmp = TempDir::new().unwrap();
        let s = Storage::open_in_memory(100).unwrap();
        // An image row with no OCR text yet (content = None).
        let mut r = row(Utc::now(), Kind::Image, 1, "", tmp.path());
        r.content = None;
        s.insert(&r).unwrap();
        // Searching for *anything* should not crash and should return
        // no rows for unrelated terms.
        let hits = s.search("anything", 10, None).unwrap();
        assert!(hits.is_empty());
    }

    /// Real-world bench: insert N captures into a fresh DB, time the
    /// inserts (which include the MD file append), then time several
    /// representative searches. Reports MD file size at end. Run with:
    ///
    ///   cargo test --bin tl bench_storage_at_scale --release \
    ///       -- --ignored --nocapture
    #[test]
    #[ignore = "perf benchmark — run with --ignored --nocapture"]
    fn bench_storage_at_scale() {
        use std::time::Instant;

        let n: usize = std::env::var("BENCH_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000);
        let tmp = TempDir::new().unwrap();
        // ring_buffer_size > N so we measure full-corpus search, not trim.
        let s = Storage::open(tmp.path().join("index.db"), n + 1).unwrap();

        // Synthesize varied content: some shared phrases (so FTS5 has
        // realistic term frequencies), some unique tokens.
        let mut rows = Vec::with_capacity(n);
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 0, 0, 0).unwrap();
        for i in 0..n {
            let content = format!(
                "panicked at index {i}: needle haystack stripe webhook \
                 fn calculate_total items pricing {i:08x}",
            );
            rows.push(row(
                base + chrono::Duration::seconds(i as i64),
                Kind::Text,
                (i % 251) as u8 + 1,
                Box::leak(content.into_boxed_str()),
                tmp.path(),
            ));
        }

        let t0 = Instant::now();
        for r in &rows {
            s.insert(r).expect("insert");
        }
        let insert_total = t0.elapsed();
        let per_insert_us = insert_total.as_micros() as f64 / n as f64;

        let queries = ["needle", "panicked", "stripe webhook", "calculate_total"];
        let mut search_results = Vec::new();
        for q in queries {
            let t = Instant::now();
            let hits = s.search(q, 100, None).unwrap();
            search_results.push((q, hits.len(), t.elapsed()));
        }

        let recent_t = Instant::now();
        let recent = s.get_recent(20, None).unwrap();
        let recent_dur = recent_t.elapsed();

        let md_path = tmp.path().join("2026-04-17.md");
        let md_size = std::fs::metadata(&md_path).map(|m| m.len()).unwrap_or(0);

        eprintln!("\n--- textlog perf bench ---");
        eprintln!("N captures           : {n}");
        eprintln!(
            "insert total         : {:.2}s  ({:.1} µs/row, {:.0} ops/sec)",
            insert_total.as_secs_f64(),
            per_insert_us,
            n as f64 / insert_total.as_secs_f64(),
        );
        eprintln!(
            "MD file              : {} bytes ({:.2} MB), {:.0} bytes/row",
            md_size,
            md_size as f64 / 1_048_576.0,
            md_size as f64 / n as f64,
        );
        for (q, count, dur) in &search_results {
            eprintln!(
                "search {:<22} : {} hits in {:>8.2}µs",
                format!("{q:?}"),
                count,
                dur.as_micros()
            );
        }
        eprintln!(
            "get_recent(20, None) : {} rows  in {:>8.2}µs",
            recent.len(),
            recent_dur.as_micros()
        );
        eprintln!("--------------------------\n");
    }
}
