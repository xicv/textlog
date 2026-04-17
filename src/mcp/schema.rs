//! Argument + result shapes for the `textlog__*` MCP tools.
//!
//! Inputs derive `JsonSchema` so rmcp can publish them via `tools/list`;
//! outputs derive `Serialize` so rmcp's `Json<T>` wrapper can ship them
//! back as structured content. Field naming/defaults match spec
//! §MCP Server & Tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---- argument structs -------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KindFilter {
    Text,
    Image,
    /// Sentinel meaning "no kind filter".
    Any,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetRecentArgs {
    /// Number of captures to return. Defaults to 5; capped server-side
    /// at `mcp.max_recent` (default 100).
    #[serde(default = "default_recent_n")]
    pub n: u32,
    /// Optional capture-kind filter. `Any` (or omitted) returns all
    /// kinds.
    #[serde(default)]
    pub kind: Option<KindFilter>,
}

fn default_recent_n() -> u32 {
    5
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// FTS5-style query. Words are AND-ed. Use double-quotes for phrases.
    pub query: String,
    /// Max hits to return. Defaults to 20; capped at `mcp.max_search_limit`.
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    /// Optional ISO 8601 lower bound — only return rows with `ts >= since`.
    #[serde(default)]
    pub since: Option<String>,
}

fn default_search_limit() -> u32 {
    20
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListTodayArgs {
    #[serde(default)]
    pub kind: Option<KindFilter>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ClearSinceArgs {
    /// ISO 8601 timestamp. Every capture row with `ts >= ts` is deleted.
    pub ts: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OcrImageArgs {
    /// Absolute filesystem path to the image file.
    pub path: String,
}

// ---- result structs ---------------------------------------------------

/// MCP requires tool outputs to be JSON objects (not bare arrays), so
/// list-style results are wrapped in a `captures` field.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CaptureList {
    pub captures: Vec<CaptureSummary>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResults {
    pub hits: Vec<SearchResult>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CaptureSummary {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub sha256: String,
    pub size_bytes: usize,
    /// For text rows this is the clipboard text; for images it is the
    /// OCR'd text (may be empty).
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_confidence: Option<f32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResult {
    pub capture: CaptureSummary,
    /// Set if another row in *this* result set with a smaller index
    /// already carries the same sha256 — Claude can elide the body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<i64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OcrResult {
    pub text: String,
    pub confidence: f32,
    pub block_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OcrLatestResult {
    pub text: Option<String>,
    pub confidence: Option<f32>,
    pub captured_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClearSinceResult {
    pub deleted_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_recent_defaults_when_args_missing() {
        let args: GetRecentArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(args.n, 5);
        assert!(args.kind.is_none());
    }

    #[test]
    fn search_defaults_limit_to_20() {
        let args: SearchArgs = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!(args.limit, 20);
        assert!(args.since.is_none());
    }

    #[test]
    fn kind_filter_serializes_lowercase() {
        let s = serde_json::to_string(&KindFilter::Image).unwrap();
        assert_eq!(s, "\"image\"");
        let parsed: KindFilter = serde_json::from_str("\"any\"").unwrap();
        assert_eq!(parsed, KindFilter::Any);
    }
}
