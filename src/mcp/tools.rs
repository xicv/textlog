//! `textlog__*` MCP tools, exposed to Claude Code via the rmcp stdio
//! server. Each handler is sync-storage wrapped in `spawn_blocking`
//! since `Storage::*` operate on a blocking SQLite connection.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorCode, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::config::schema::OcrConfig;
use crate::error::Error;
use crate::ocr;
use crate::storage::{hex_lower, CaptureRow, Kind, SearchHit, Storage};

use super::schema::{
    CaptureFull, CaptureList, CaptureSummary, ClearSinceArgs, ClearSinceResult, GetCaptureArgs,
    GetRecentArgs, KindFilter, ListTodayArgs, OcrImageArgs, OcrLatestResult, OcrResult,
    SearchArgs, SearchResult, SearchResults, GET_CAPTURE_DEFAULT_LIMIT, GET_CAPTURE_MAX_LIMIT,
};

/// Maximum characters of clipboard body returned in list-style MCP
/// responses. Beyond this, callers must fetch the full body via
/// `textlog__get_capture`. 200 chars ≈ 50 tokens — enough to identify
/// a capture, small enough that 5 rows still fit in ~250 tokens.
const PREVIEW_CHARS: usize = 200;

/// Slice the leading `PREVIEW_CHARS` characters off `s`, on a UTF-8
/// char boundary. Returns the prefix and a flag indicating truncation.
fn truncate_to_preview(s: &str) -> (String, bool) {
    let mut out = String::new();
    let mut iter = s.chars();
    for _ in 0..PREVIEW_CHARS {
        match iter.next() {
            Some(c) => out.push(c),
            None => return (out, false),
        }
    }
    let truncated = iter.next().is_some();
    (out, truncated)
}

/// MCP server state — owns the Storage handle and rmcp's tool router.
#[derive(Clone)]
pub struct McpServer {
    storage: Arc<Storage>,
    ocr_cfg: Arc<OcrConfig>,
    #[allow(dead_code)] // populated by #[tool_router] macro; read via ServerHandler impl
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl McpServer {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self::with_ocr(storage, OcrConfig::default())
    }

    pub fn with_ocr(storage: Arc<Storage>, ocr_cfg: OcrConfig) -> Self {
        Self {
            storage,
            ocr_cfg: Arc::new(ocr_cfg),
            tool_router: Self::tool_router(),
        }
    }

    /// Most recent N captures, deduplicated by sha256.
    #[tool(
        name = "textlog__get_recent",
        description = "Fetch the N most recent clipboard captures (deduplicated by SHA-256). \
                       For images, the `text` field carries the OCR result captured at the time."
    )]
    pub async fn get_recent(
        &self,
        Parameters(args): Parameters<GetRecentArgs>,
    ) -> Result<Json<CaptureList>, ErrorData> {
        let storage = Arc::clone(&self.storage);
        let kind = filter_to_kind(args.kind);
        let n = args.n;
        let rows = blocking(move || storage.get_recent(n, kind)).await?;
        Ok(Json(CaptureList {
            captures: rows.into_iter().map(capture_summary).collect(),
        }))
    }

    /// Today's captures (UTC midnight cutoff), deduplicated by sha256.
    #[tool(
        name = "textlog__list_today",
        description = "Return every capture from today (UTC), deduplicated by SHA-256."
    )]
    pub async fn list_today(
        &self,
        Parameters(args): Parameters<ListTodayArgs>,
    ) -> Result<Json<CaptureList>, ErrorData> {
        let storage = Arc::clone(&self.storage);
        let kind = filter_to_kind(args.kind);
        let cutoff = today_midnight_utc();
        let rows = blocking(move || storage.get_recent(u32::MAX, kind)).await?;
        let captures = rows
            .into_iter()
            .filter(|r| r.ts >= cutoff)
            .map(capture_summary)
            .collect();
        Ok(Json(CaptureList { captures }))
    }

    /// FTS5 search over the SQLite index.
    #[tool(
        name = "textlog__search",
        description = "Full-text search over captured content (FTS5 syntax). \
                       Hits sharing a SHA-256 with an earlier hit in the result set \
                       are marked with `duplicate_of` so the body can be elided."
    )]
    pub async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<Json<SearchResults>, ErrorData> {
        let storage = Arc::clone(&self.storage);
        let SearchArgs { query, limit, since } = args;
        let since_dt = parse_since(since.as_deref())?;
        let hits = blocking(move || storage.search(&query, limit, since_dt)).await?;
        Ok(Json(SearchResults {
            hits: hits.into_iter().map(search_result).collect(),
        }))
    }

    /// OCR text from the most recent image capture (no fresh OCR call).
    #[tool(
        name = "textlog__ocr_latest",
        description = "Return the OCR text recorded for the most recent image capture, \
                       or null fields if no image has been captured yet."
    )]
    pub async fn ocr_latest(&self) -> Result<Json<OcrLatestResult>, ErrorData> {
        let storage = Arc::clone(&self.storage);
        let row = blocking(move || storage.get_latest_image()).await?;
        Ok(Json(match row {
            Some(r) => OcrLatestResult {
                text: r.content,
                confidence: r.ocr_confidence,
                captured_at: Some(r.ts.to_rfc3339()),
            },
            None => OcrLatestResult {
                text: None,
                confidence: None,
                captured_at: None,
            },
        }))
    }

    /// Privacy cut-off — drop SQLite rows at or after `ts`.
    #[tool(
        name = "textlog__clear_since",
        description = "Delete every capture row with `ts >= ts` (ISO 8601). \
                       Daily Markdown files on disk are not modified."
    )]
    pub async fn clear_since(
        &self,
        Parameters(args): Parameters<ClearSinceArgs>,
    ) -> Result<Json<ClearSinceResult>, ErrorData> {
        let storage = Arc::clone(&self.storage);
        let ts = parse_iso8601(&args.ts)?;
        let deleted = blocking(move || storage.clear_since(ts)).await?;
        Ok(Json(ClearSinceResult { deleted_count: deleted }))
    }

    /// Fetch a windowed slice of a single capture body by `id`. Used
    /// to expand a truncated preview returned by `get_recent` /
    /// `list_today` / `search` without re-paying their list-shape
    /// token cost. Caller pages via `offset` + `limit` so a 50K-char
    /// body never overflows the MCP per-tool token budget.
    #[tool(
        name = "textlog__get_capture",
        description = "Return a slice of the clipboard body for the capture with the given `id`. \
                       `offset` (default 0) and `limit` (default 8000 chars, max 32000) page \
                       through the body — paginate with `offset = text_offset + \
                       text.chars().count()` until `truncated` is false. Errors with \
                       INVALID_PARAMS if no row matches (e.g. trimmed by the ring buffer)."
    )]
    pub async fn get_capture(
        &self,
        Parameters(args): Parameters<GetCaptureArgs>,
    ) -> Result<Json<CaptureFull>, ErrorData> {
        let storage = Arc::clone(&self.storage);
        let id = args.id;
        let offset = args.offset.unwrap_or(0);
        let limit = args
            .limit
            .unwrap_or(GET_CAPTURE_DEFAULT_LIMIT)
            .min(GET_CAPTURE_MAX_LIMIT);
        let row = blocking(move || storage.get_by_id(id)).await?;
        match row {
            Some(r) => Ok(Json(capture_full(r, offset, limit))),
            None => Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("no capture with id {id} (may have been trimmed)"),
                None,
            )),
        }
    }

    /// Ad-hoc OCR of an image file outside the clipboard stream.
    #[tool(
        name = "textlog__ocr_image",
        description = "Run Apple Vision OCR on the image file at `path` (absolute path). \
                       Returns the recognized text, mean block confidence, and block count."
    )]
    pub async fn ocr_image(
        &self,
        Parameters(args): Parameters<OcrImageArgs>,
    ) -> Result<Json<OcrResult>, ErrorData> {
        let cfg = Arc::clone(&self.ocr_cfg);
        let path = args.path;
        let res = tokio::task::spawn_blocking(move || -> crate::error::Result<_> {
            let bytes = std::fs::read(&path)?;
            ocr::ocr_image(&bytes, &cfg)
        })
        .await
        .map_err(|e| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("ocr task join error: {e}"),
                None,
            )
        })?
        .map_err(storage_error_to_data)?;
        Ok(Json(OcrResult {
            text: res.text,
            confidence: res.confidence,
            block_count: res.block_count,
        }))
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo and Implementation are #[non_exhaustive] — mutate
        // a Default rather than construct via struct expression.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "textlog: macOS clipboard + OCR archive. ALWAYS use textlog__* tools — \
             never shell out to `tl` for reads (CLI is for daemon lifecycle only). \
             Recipes: latest clipboard → textlog__get_recent(n=1); last few items → \
             textlog__get_recent(n=5); today's history → textlog__list_today; keyword \
             lookup → textlog__search(query, limit=10); latest image OCR → \
             textlog__ocr_latest. List tools return a 200-char `text_preview` + \
             `truncated` flag; only call textlog__get_capture(id) when truncated=true \
             and you need the full body. Skip `tl --help` / `tl logs` — same data, more tokens."
                .into(),
        );
        info
    }
}

// ---- helpers ---------------------------------------------------------

fn filter_to_kind(filter: Option<KindFilter>) -> Option<Kind> {
    match filter {
        None | Some(KindFilter::Any) => None,
        Some(KindFilter::Text) => Some(Kind::Text),
        Some(KindFilter::Image) => Some(Kind::Image),
    }
}

fn capture_summary(row: CaptureRow) -> CaptureSummary {
    let (text_preview, truncated) = match row.content.as_deref() {
        Some(s) => {
            let (prefix, t) = truncate_to_preview(s);
            (Some(prefix), t)
        }
        None => (None, false),
    };
    CaptureSummary {
        id: row.id,
        ts: row.ts.to_rfc3339(),
        kind: row.kind.as_str().to_string(),
        sha256: hex_lower(&row.sha256),
        size_bytes: row.size_bytes,
        text_preview,
        truncated,
        md_path: row.md_path.to_string_lossy().into_owned(),
        source_app: row.source_app,
        source_url: row.source_url,
        ocr_confidence: row.ocr_confidence,
    }
}

fn capture_full(row: CaptureRow, offset: usize, limit: usize) -> CaptureFull {
    let (text, text_offset, text_total_chars, truncated) = match row.content.as_deref() {
        Some(body) => {
            let total = body.chars().count();
            let start = offset.min(total);
            let end = start.saturating_add(limit).min(total);
            let slice: String = body.chars().skip(start).take(end - start).collect();
            (Some(slice), start, total, end < total)
        }
        None => (None, 0, 0, false),
    };
    CaptureFull {
        id: row.id,
        ts: row.ts.to_rfc3339(),
        kind: row.kind.as_str().to_string(),
        sha256: hex_lower(&row.sha256),
        size_bytes: row.size_bytes,
        text,
        text_offset,
        text_total_chars,
        truncated,
        md_path: row.md_path.to_string_lossy().into_owned(),
        source_app: row.source_app,
        source_url: row.source_url,
        ocr_confidence: row.ocr_confidence,
    }
}

fn search_result(hit: SearchHit) -> SearchResult {
    SearchResult {
        duplicate_of: hit.duplicate_of,
        capture: capture_summary(hit.row),
    }
}

fn today_midnight_utc() -> DateTime<Utc> {
    let now = Utc::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc()
}

fn parse_since(since: Option<&str>) -> Result<Option<DateTime<Utc>>, ErrorData> {
    since.map(parse_iso8601).transpose()
}

fn parse_iso8601(s: &str) -> Result<DateTime<Utc>, ErrorData> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("invalid ISO 8601 timestamp {s:?}: {e}"),
                None,
            )
        })
}

/// Move sync storage work off the executor thread.
async fn blocking<T, F>(f: F) -> Result<T, ErrorData>
where
    T: Send + 'static,
    F: FnOnce() -> crate::error::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("storage task join error: {e}"),
                None,
            )
        })?
        .map_err(storage_error_to_data)
}

fn storage_error_to_data(e: Error) -> ErrorData {
    let code = match e {
        Error::Storage(_) | Error::Sqlite(_) | Error::Io(_) => ErrorCode::INTERNAL_ERROR,
        _ => ErrorCode::INTERNAL_ERROR,
    };
    ErrorData::new(code, e.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::CaptureRow;
    use chrono::TimeZone;
    use std::path::Path;
    use tempfile::TempDir;

    fn server_with_storage() -> (McpServer, TempDir) {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::open_in_memory(100).unwrap();
        (McpServer::new(Arc::new(storage)), tmp)
    }

    fn row(
        ts: DateTime<Utc>,
        kind: Kind,
        sha: u8,
        content: Option<&str>,
        md_dir: &Path,
    ) -> CaptureRow {
        CaptureRow {
            id: 0,
            ts,
            kind,
            sha256: [sha; 32],
            size_bytes: content.map(|c| c.len()).unwrap_or(0),
            content: content.map(String::from),
            ocr_confidence: matches!(kind, Kind::Image).then_some(0.9),
            source_app: None,
            source_url: None,
            md_path: md_dir.join("2026-04-17.md"),
        }
    }

    #[tokio::test]
    async fn get_recent_returns_empty_when_no_captures() {
        let (server, _tmp) = server_with_storage();
        let res = server
            .get_recent(Parameters(GetRecentArgs { n: 5, kind: None }))
            .await
            .unwrap();
        assert!(res.0.captures.is_empty());
    }

    #[tokio::test]
    async fn get_recent_returns_summary_with_kind_filter() {
        let (server, tmp) = server_with_storage();
        let now = Utc::now();
        server
            .storage
            .insert(&row(now, Kind::Text, 1, Some("text 1"), tmp.path()))
            .unwrap();
        server
            .storage
            .insert(&row(
                now + chrono::Duration::seconds(1),
                Kind::Image,
                2,
                Some("ocr text"),
                tmp.path(),
            ))
            .unwrap();

        let images = server
            .get_recent(Parameters(GetRecentArgs {
                n: 10,
                kind: Some(KindFilter::Image),
            }))
            .await
            .unwrap();
        let cs = &images.0.captures;
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, "image");
        assert_eq!(cs[0].text_preview.as_deref(), Some("ocr text"));
        assert!(!cs[0].truncated, "short body must not be flagged truncated");
        assert!(cs[0].sha256.starts_with("0202"));
    }

    #[tokio::test]
    async fn get_recent_kind_any_returns_all() {
        let (server, tmp) = server_with_storage();
        let now = Utc::now();
        server.storage.insert(&row(now, Kind::Text, 1, Some("a"), tmp.path())).unwrap();
        server.storage.insert(&row(now + chrono::Duration::seconds(1), Kind::Image, 2, Some("b"), tmp.path())).unwrap();

        let res = server
            .get_recent(Parameters(GetRecentArgs {
                n: 10,
                kind: Some(KindFilter::Any),
            }))
            .await
            .unwrap();
        assert_eq!(res.0.captures.len(), 2);
    }

    #[tokio::test]
    async fn search_returns_hits_with_duplicate_of() {
        let (server, tmp) = server_with_storage();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        // Two rows, same sha256, both contain "needle".
        server
            .storage
            .insert(&row(base, Kind::Text, 7, Some("needle alpha"), tmp.path()))
            .unwrap();
        server
            .storage
            .insert(&row(
                base + chrono::Duration::seconds(60),
                Kind::Text,
                7,
                Some("needle alpha"),
                tmp.path(),
            ))
            .unwrap();

        let res = server
            .search(Parameters(SearchArgs {
                query: "needle".into(),
                limit: 10,
                since: None,
            }))
            .await
            .unwrap();
        let hits = &res.0.hits;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].duplicate_of, None, "first hit is canonical");
        assert!(hits[1].duplicate_of.is_some(), "second hit points back");
    }

    #[tokio::test]
    async fn search_rejects_invalid_since() {
        let (server, _tmp) = server_with_storage();
        let err = server
            .search(Parameters(SearchArgs {
                query: "x".into(),
                limit: 5,
                since: Some("not a date".into()),
            }))
            .await
            .err()
            .expect("expected an error");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn ocr_latest_returns_null_when_no_image() {
        let (server, _tmp) = server_with_storage();
        let res = server.ocr_latest().await.unwrap();
        assert!(res.0.text.is_none());
        assert!(res.0.captured_at.is_none());
    }

    #[tokio::test]
    async fn ocr_latest_returns_text_from_latest_image() {
        let (server, tmp) = server_with_storage();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        server
            .storage
            .insert(&row(base, Kind::Image, 1, Some("first ocr"), tmp.path()))
            .unwrap();
        server
            .storage
            .insert(&row(
                base + chrono::Duration::seconds(60),
                Kind::Image,
                2,
                Some("latest ocr"),
                tmp.path(),
            ))
            .unwrap();

        let res = server.ocr_latest().await.unwrap();
        assert_eq!(res.0.text.as_deref(), Some("latest ocr"));
        assert!(res.0.confidence.is_some());
        assert!(res.0.captured_at.is_some());
    }

    #[tokio::test]
    async fn clear_since_returns_count() {
        let (server, tmp) = server_with_storage();
        let base = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        server
            .storage
            .insert(&row(base, Kind::Text, 1, Some("before"), tmp.path()))
            .unwrap();
        server
            .storage
            .insert(&row(
                base + chrono::Duration::seconds(60),
                Kind::Text,
                2,
                Some("after"),
                tmp.path(),
            ))
            .unwrap();

        let cutoff = (base + chrono::Duration::seconds(30)).to_rfc3339();
        let res = server
            .clear_since(Parameters(ClearSinceArgs { ts: cutoff }))
            .await
            .unwrap();
        assert_eq!(res.0.deleted_count, 1, "only the row at +60s deleted");
    }

    #[tokio::test]
    async fn clear_since_rejects_invalid_ts() {
        let (server, _tmp) = server_with_storage();
        let err = server
            .clear_since(Parameters(ClearSinceArgs {
                ts: "yesterday".into(),
            }))
            .await
            .err()
            .expect("expected an error");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn list_today_returns_only_todays_rows() {
        let (server, tmp) = server_with_storage();
        // Insert one row from yesterday and one from today.
        let yesterday = Utc::now() - chrono::Duration::days(1);
        let today = Utc::now();
        server
            .storage
            .insert(&row(yesterday, Kind::Text, 1, Some("old"), tmp.path()))
            .unwrap();
        server
            .storage
            .insert(&row(today, Kind::Text, 2, Some("fresh"), tmp.path()))
            .unwrap();

        let res = server
            .list_today(Parameters(ListTodayArgs { kind: None }))
            .await
            .unwrap();
        let cs = &res.0.captures;
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].text_preview.as_deref(), Some("fresh"));
        assert!(!cs[0].truncated);
    }

    #[tokio::test]
    async fn ocr_image_returns_io_error_for_missing_path() {
        let (server, tmp) = server_with_storage();
        let missing = tmp.path().join("does-not-exist.png");
        let err = server
            .ocr_image(Parameters(OcrImageArgs {
                path: missing.to_string_lossy().into_owned(),
            }))
            .await
            .err()
            .expect("expected an error");
        // std::fs::read on a missing file → Error::Io → INTERNAL_ERROR.
        let msg = format!("{}", err.message);
        assert!(msg.contains("I/O") || msg.contains("No such"), "got: {msg}");
    }

    #[test]
    fn server_info_advertises_tools_capability() {
        let (server, _tmp) = server_with_storage();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.instructions.unwrap().contains("textlog"));
    }

    #[test]
    fn truncate_short_string_is_not_marked_truncated() {
        let (out, t) = truncate_to_preview("hello");
        assert_eq!(out, "hello");
        assert!(!t);
    }

    #[test]
    fn truncate_long_string_caps_at_preview_chars_and_flags() {
        let big = "a".repeat(PREVIEW_CHARS * 3);
        let (out, t) = truncate_to_preview(&big);
        assert_eq!(out.chars().count(), PREVIEW_CHARS);
        assert!(t);
    }

    #[test]
    fn truncate_handles_multibyte_chars_on_boundary() {
        // Each "é" is 2 bytes / 1 char. Build a string longer than the
        // preview limit; ensure we don't slice mid-codepoint.
        let s = "é".repeat(PREVIEW_CHARS + 50);
        let (out, t) = truncate_to_preview(&s);
        assert!(t);
        assert_eq!(out.chars().count(), PREVIEW_CHARS);
        // Result must be valid UTF-8 (String guarantees this, but the
        // count-by-chars assertion above also catches mid-codepoint cuts).
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[tokio::test]
    async fn get_recent_truncates_long_body_and_flags() {
        let (server, tmp) = server_with_storage();
        let big = "x".repeat(PREVIEW_CHARS * 5);
        server
            .storage
            .insert(&row(Utc::now(), Kind::Text, 1, Some(&big), tmp.path()))
            .unwrap();

        let res = server
            .get_recent(Parameters(GetRecentArgs { n: 5, kind: None }))
            .await
            .unwrap();
        let cs = &res.0.captures;
        assert_eq!(cs.len(), 1);
        let preview = cs[0].text_preview.as_deref().unwrap();
        assert_eq!(preview.chars().count(), PREVIEW_CHARS, "preview must be capped");
        assert!(cs[0].truncated, "long body must set truncated=true");
        assert_eq!(cs[0].size_bytes, big.len(), "size_bytes reports full length");
    }

    #[tokio::test]
    async fn get_capture_returns_full_body_when_within_default_window() {
        let (server, tmp) = server_with_storage();
        let big = "y".repeat(PREVIEW_CHARS * 4);
        let id = server
            .storage
            .insert(&row(Utc::now(), Kind::Text, 1, Some(&big), tmp.path()))
            .unwrap();

        let res = server
            .get_capture(Parameters(GetCaptureArgs {
                id,
                offset: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.0.id, id);
        assert_eq!(res.0.text.as_deref(), Some(big.as_str()));
        assert_eq!(res.0.text_offset, 0);
        assert_eq!(res.0.text_total_chars, big.chars().count());
        assert!(!res.0.truncated);
    }

    #[tokio::test]
    async fn get_capture_pages_through_body_with_offset_and_limit() {
        let (server, tmp) = server_with_storage();
        // 20_000 chars — over default 8000 window, under 32000 cap.
        let big: String = (0..20_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let id = server
            .storage
            .insert(&row(Utc::now(), Kind::Text, 1, Some(&big), tmp.path()))
            .unwrap();

        // Page 1: default window.
        let p1 = server
            .get_capture(Parameters(GetCaptureArgs {
                id,
                offset: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert_eq!(p1.0.text_offset, 0);
        assert_eq!(p1.0.text.as_ref().unwrap().chars().count(), 8000);
        assert_eq!(p1.0.text_total_chars, 20_000);
        assert!(p1.0.truncated);

        // Page 2: continue from end of page 1.
        let next_offset = p1.0.text_offset + p1.0.text.as_ref().unwrap().chars().count();
        let p2 = server
            .get_capture(Parameters(GetCaptureArgs {
                id,
                offset: Some(next_offset),
                limit: Some(8000),
            }))
            .await
            .unwrap();
        assert_eq!(p2.0.text_offset, 8000);
        assert_eq!(p2.0.text.as_ref().unwrap().chars().count(), 8000);
        assert!(p2.0.truncated);

        // Page 3: tail.
        let p3 = server
            .get_capture(Parameters(GetCaptureArgs {
                id,
                offset: Some(16_000),
                limit: Some(8000),
            }))
            .await
            .unwrap();
        assert_eq!(p3.0.text_offset, 16_000);
        assert_eq!(p3.0.text.as_ref().unwrap().chars().count(), 4000);
        assert!(!p3.0.truncated);
    }

    #[tokio::test]
    async fn get_capture_caps_oversize_limit_to_max() {
        let (server, tmp) = server_with_storage();
        let big = "z".repeat(50_000);
        let id = server
            .storage
            .insert(&row(Utc::now(), Kind::Text, 1, Some(&big), tmp.path()))
            .unwrap();

        let res = server
            .get_capture(Parameters(GetCaptureArgs {
                id,
                offset: None,
                limit: Some(usize::MAX),
            }))
            .await
            .unwrap();
        // Caller asked for usize::MAX, server clamped to 32000.
        assert_eq!(res.0.text.as_ref().unwrap().chars().count(), 32_000);
        assert!(res.0.truncated);
    }

    #[tokio::test]
    async fn get_capture_offset_past_end_returns_empty_slice() {
        let (server, tmp) = server_with_storage();
        let body = "small";
        let id = server
            .storage
            .insert(&row(Utc::now(), Kind::Text, 1, Some(body), tmp.path()))
            .unwrap();

        let res = server
            .get_capture(Parameters(GetCaptureArgs {
                id,
                offset: Some(9_999),
                limit: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.0.text.as_deref(), Some(""));
        assert_eq!(res.0.text_offset, body.chars().count());
        assert!(!res.0.truncated);
    }

    #[tokio::test]
    async fn get_capture_errors_on_missing_id() {
        let (server, _tmp) = server_with_storage();
        let err = server
            .get_capture(Parameters(GetCaptureArgs {
                id: 9_999_999,
                offset: None,
                limit: None,
            }))
            .await
            .err()
            .expect("expected an error");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }
}
