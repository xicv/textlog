//! `textlog__*` MCP tools, exposed to Claude Code via the rmcp stdio
//! server. Each handler is sync-storage wrapped in `spawn_blocking`
//! since `Storage::*` operate on a blocking SQLite connection.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorCode, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::error::Error;
use crate::storage::{hex_lower, CaptureRow, Kind, SearchHit, Storage};

use super::schema::{
    CaptureList, CaptureSummary, ClearSinceArgs, ClearSinceResult, GetRecentArgs, KindFilter,
    ListTodayArgs, OcrImageArgs, OcrLatestResult, OcrResult, SearchArgs, SearchResult,
    SearchResults,
};

/// MCP server state — owns the Storage handle and rmcp's tool router.
#[derive(Clone)]
pub struct McpServer {
    storage: Arc<Storage>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl McpServer {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
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

    /// Ad-hoc OCR of an image file outside the clipboard stream.
    /// Stubbed until Phase 6 Task 7a (OCR module) lands.
    #[tool(
        name = "textlog__ocr_image",
        description = "Run Apple Vision OCR on an image file at the given absolute path. \
                       Currently unimplemented — returns an error until the OCR module ships."
    )]
    pub async fn ocr_image(
        &self,
        Parameters(_args): Parameters<OcrImageArgs>,
    ) -> Result<Json<OcrResult>, ErrorData> {
        Err(ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            "textlog__ocr_image is not yet implemented (Phase 6 Task 7a pending)",
            None,
        ))
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
            "textlog: clipboard + OCR archive accessed via textlog__* tools. \
             Use textlog__get_recent for the latest items, textlog__search for FTS5 lookup, \
             textlog__ocr_latest for the last image's OCR text."
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
    CaptureSummary {
        id: row.id,
        ts: row.ts.to_rfc3339(),
        kind: row.kind.as_str().to_string(),
        sha256: hex_lower(&row.sha256),
        size_bytes: row.size_bytes,
        text: row.content,
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
        assert_eq!(cs[0].text.as_deref(), Some("ocr text"));
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
        assert_eq!(cs[0].text.as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn ocr_image_returns_unimplemented_error() {
        let (server, _tmp) = server_with_storage();
        let err = server
            .ocr_image(Parameters(OcrImageArgs {
                path: "/tmp/x.png".into(),
            }))
            .await
            .err()
            .expect("expected an error");
        assert!(err.message.contains("not yet implemented"));
    }

    #[test]
    fn server_info_advertises_tools_capability() {
        let (server, _tmp) = server_with_storage();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.instructions.unwrap().contains("textlog"));
    }
}
