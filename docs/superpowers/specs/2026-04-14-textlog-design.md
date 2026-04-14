# textlog Design Specification

**Date**: 2026-04-14
**Status**: Draft — v2.0 (pivoted to MCP server + Apple Vision OCR)
**Author**: Design collaboration between user and Claude

---

## Executive Summary

`textlog` (command: `tl`) is a macOS-native background daemon that captures every clipboard event, OCRs images on-device via Apple Vision Framework, and exposes the result to [Claude Code](https://code.claude.com) as a **Model Context Protocol (MCP) server**.

It is **not** a clipboard manager, a RAG engine, or a parallel LLM stack. It is a Claude Code *context pipeline*. When you copy a stack trace, a screenshot, or a log fragment, Claude Code can pull it into the conversation via `textlog__get_recent` or `textlog__ocr_latest` without you pasting anything.

### Why this shape

Four concrete goals drive the design:

| Goal | How textlog addresses it |
|---|---|
| Save tokens / stretch the Pro plan 44K-tokens/5h window | Local Apple Vision OCR turns 2 000-token screenshots into 30–300-token text; SHA-256 dedup avoids re-sending identical captures; Claude fetches on demand instead of receiving redundant pastes |
| Eliminate copy-paste hell in the dev loop | Claude Code calls `textlog__get_recent(n)` as a tool; the human never pastes |
| Work around Claude Code's image limits (30 MB per file, ~5 images/request) | OCR'd text can be batched by the tens without hitting size or count caps |
| Faster dev loop | Zero context-switch — copy something, keep talking to Claude, it sees the clipboard |

---

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [CLI Commands](#cli-commands)
3. [MCP Server & Tools](#mcp-server--tools)
4. [Configuration](#configuration)
5. [Notifications](#notifications)
6. [macOS Permissions](#macos-permissions)
7. [Data Format](#data-format)
8. [Technology Stack](#technology-stack)
9. [Security & Privacy](#security--privacy)
10. [Error Handling](#error-handling)
11. [Testing Strategy](#testing-strategy)

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                       textlog daemon (Rust, LaunchAgent)                    │
│                                                                             │
│   ┌──────────────────┐   ┌──────────────────┐   ┌──────────────────────┐  │
│   │ Clipboard        │   │ Config Manager   │   │ Notifier             │  │
│   │ changeCount poll │   │ TOML + env       │   │ notify-rust          │  │
│   │ detect() gating  │   │ hot-reload SIGHUP│   │ on_capture (opt-in)  │  │
│   │ self-write skip  │   │                  │   │ on_complete + path   │  │
│   └────────┬─────────┘   └──────────────────┘   └──────────────────────┘  │
│            │                                                                │
│            ▼                                                                │
│   ┌──────────────────────────┐     ┌───────────────────────────────────┐  │
│   │ Pipeline (async tokio)   │────▶│ OCR (Apple Vision Framework)      │  │
│   │ filter → OCR → persist   │     │ VNRecognizeTextRequest, on-device │  │
│   └────────────┬─────────────┘     │ <50 ms, zero network, free        │  │
│                │                   └───────────────────────────────────┘  │
│                ▼                                                            │
│   ┌─────────────────────────────────────────────────────────────────────┐  │
│   │ Storage                                                              │ │
│   │  • SQLite ring buffer  (~/textlog/index.db)     last 1 000 captures │ │
│   │     — indexed by timestamp, sha256, kind (text|image) for MCP reads │ │
│   │  • Daily Markdown      (~/textlog/logs/YYYY-MM-DD.md) human archive │ │
│   └─────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │ stdio (JSON-RPC 2.0)
                                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                           Claude Code (user session)                        │
│                                                                             │
│    calls tools:                                                             │
│      textlog__get_recent(n, kind?)     → last N captures as text            │
│      textlog__search(query, limit)     → substring/regex over archive       │
│      textlog__ocr_image(path)          → one-shot OCR for ad-hoc files      │
│      textlog__ocr_latest()             → OCR text from most recent image    │
│      textlog__list_today()             → all of today's captures            │
│      textlog__clear_since(ts)          → drop captures after ts (privacy)   │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Module Boundaries

| Module | Responsibility | Depends on |
|---|---|---|
| `clipboard` | NSPasteboard polling, `detect()`-gated reads, self-write suppression | `config`, `macos_perm` |
| `config` | TOML parse + env overlay + schema v1 | — |
| `ocr` | Apple Vision `VNRecognizeTextRequest` wrapper | — |
| `storage` | SQLite ring buffer + daily Markdown writer | `config` |
| `pipeline` | Orchestration: filter → OCR → storage → notifier | all above |
| `notifier` | `notify-rust` dispatch + clipboard path copy + self-write token | `config`, `storage` |
| `mcp` | JSON-RPC 2.0 stdio server exposing `textlog__*` tools | `storage`, `ocr` |
| `service` | LaunchAgent plist generation + `launchctl` lifecycle | `config` |
| `macos_perm` | Pasteboard / notification permission checks for `tl doctor` | — |
| `cli` | clap command layer | `pipeline`, `config`, `service`, `mcp` |

No module contains provider- or model-specific logic. The LLM lives in Claude Code; textlog never talks to it directly.

---

## CLI Commands

### LaunchAgent lifecycle

```bash
tl install                 # Write ~/Library/LaunchAgents/com.textlog.agent.plist + bootstrap
tl install --auto-start    # Also set RunAtLoad = true (start on login)
tl uninstall               # bootout + remove plist
tl start                   # launchctl kickstart
tl stop                    # launchctl kill SIGTERM
tl start --foreground      # Dev mode: run inline, no launchctl
tl start --daemon          # Internal — used only by launchd, never by user
tl status                  # Service + recent activity
```

### MCP server

```bash
tl mcp                     # Run the MCP server on stdio (invoked by Claude Code)

# One-time registration with Claude Code:
claude mcp add textlog -- /usr/local/bin/tl mcp
```

Once registered, Claude Code auto-discovers the `textlog__*` tools and can call them during any conversation.

### Configuration

```bash
tl config get [key]
tl config set <key> <val>
tl config edit
tl config reset
```

### Log management (human-facing)

```bash
tl logs                    # Print today's markdown via pager
tl logs --date 2026-04-14
tl logs --tail 50
```

### Utility

```bash
tl doctor                  # Health check (permissions, service, SQLite, MCP registration)
tl version
tl help
```

### LaunchAgent plist (generated)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "...">
<plist version="1.0">
<dict>
    <key>Label</key>             <string>com.textlog.agent</string>
    <key>ProgramArguments</key>  <array>
                                   <string>/usr/local/bin/tl</string>
                                   <string>--daemon</string>
                                 </array>
    <key>RunAtLoad</key>         <false/>
    <key>KeepAlive</key>         <true/>
    <key>StandardOutPath</key>   <string>~/Library/Logs/textlog.log</string>
    <key>StandardErrorPath</key> <string>~/Library/Logs/textlog.err</string>
</dict>
</plist>
```

`--daemon` is identical to `--foreground` except tracing is routed to those two files.

---

## MCP Server & Tools

textlog implements [Model Context Protocol](https://modelcontextprotocol.io) version **2025-06-18** (the current spec as of April 2026) over the **stdio transport**: Claude Code spawns `tl mcp` as a subprocess and exchanges JSON-RPC 2.0 messages on stdin/stdout.

### Transport

- JSON-RPC 2.0, UTF-8, line-delimited.
- Server initialisation advertises `tools/listChanged` capability so the daemon can surface new tools without a restart.
- Implementation: the Rust SDK [`rmcp`](https://crates.io/crates/rmcp) (official MCP Rust crate).

### Exposed tools

| Tool | Arguments | Returns | Purpose |
|---|---|---|---|
| `textlog__get_recent` | `n: u32 (default 5, max 100)`, `kind?: "text"\|"image"\|"any"` | array of `{ts, kind, sha256, text}` | Fetch the N most recent captures. For images, `text` is the OCR result. |
| `textlog__search` | `query: string`, `limit?: u32 (default 20)`, `since?: ISO8601` | array of matches with context lines | Substring + optional regex (`regex:` prefix) search over the SQLite index. |
| `textlog__ocr_image` | `path: string (absolute filesystem path)` | `{text, confidence, block_count}` | Ad-hoc OCR of a file outside the clipboard stream. |
| `textlog__ocr_latest` | none | `{text, confidence, captured_at}` | Short-circuit for "what text was in the last image I copied?". |
| `textlog__list_today` | `kind?` | array of captures | All of today's entries. |
| `textlog__clear_since` | `ts: ISO8601` | `{deleted_count}` | Privacy cut-off — remove SQLite rows after a point. Markdown files untouched (user can delete manually). |

### Dedup contract

Every capture stores `sha256(content)`. `get_recent` and `list_today` elide duplicates by hash by default; `search` returns all matches with a `duplicate_of` field when relevant. This means Claude receives each unique piece of clipboard content only once per tool call — core to the token-savings goal.

### Example tool call

```jsonc
// Claude → textlog
{ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
  "params": { "name": "textlog__get_recent", "arguments": { "n": 3, "kind": "any" } } }

// textlog → Claude
{ "jsonrpc": "2.0", "id": 7, "result": { "content": [
    { "type": "text", "text": "[1] 2026-04-14T21:02:11 text (sha 1a2b…) — thread 'main' panicked at 'index out of bounds'…" },
    { "type": "text", "text": "[2] 2026-04-14T20:58:03 image (sha 9f3c…) — OCR: \"error: no space left on device\"" },
    { "type": "text", "text": "[3] 2026-04-14T20:54:47 text (sha 4d7e…) — fn calculate_total(items: &[Item]) -> u64 { … }" }
] } }
```

---

## Configuration

### Default file location

`~/textlog/config.toml` (overridable via `TEXTLOG_CONFIG_DIR`).

### Complete default configuration

```toml
# textlog v2.0 — every key documented. Regenerated by `tl config reset`.
schema_version = 2

[monitoring]
enabled = true
poll_interval_ms = 250
min_length = 10
ignore_patterns = [
    "^sk-[A-Za-z0-9]{20,}",
    "^\\w+_KEY\\s*=",
    "\\b\\d{4}[- ]?\\d{4}[- ]?\\d{4}[- ]?\\d{4}\\b",
    "(?i)password\\s*[=:]\\s*\\S+",
]
ignore_own_log_paths = true

[ocr]
enabled = true
recognition_level = "accurate"    # "accurate" | "fast"  — Apple Vision level
languages = ["en-US"]             # Vision framework language hints
min_confidence = 0.4              # Drop OCR text below this confidence
image_max_dimension = 4096        # Downscale larger images before OCR (performance)

[storage]
log_dir = "~/textlog/logs"
sqlite_path = "~/textlog/index.db"
ring_buffer_size = 1000           # Captures kept in SQLite; older rolled off
date_format = "%Y-%m-%d"
max_md_file_size_mb = 10          # Rotation deferred to v2.1

[privacy]
filter_enabled = true
log_sensitive = false
show_filter_notification = true

[notifications]
enabled = true
on_capture = false
on_complete = true
copy_log_path_on_complete = true
sound = false

[mcp]
# stdio server runs only when `tl mcp` is invoked; no config needed for transport.
# Tool limits:
max_recent = 100                  # cap on n in get_recent
max_search_limit = 200            # cap on limit in search
max_search_results_bytes = 65536  # truncate oversized results

[ui]
pager = "less"
color_output = "auto"
timestamp_format = "%H:%M:%S"

[log]
level = "info"
format = "pretty"
```

### Environment variable overlay

```
TEXTLOG_CONFIG_DIR          override default config directory
TEXTLOG_LOG_DIR             override storage.log_dir
TEXTLOG_SQLITE_PATH         override storage.sqlite_path
```

Precedence: defaults → config file → env vars → CLI flags.

---

## Notifications

Dispatched via `notify-rust` (routes to `mac-notification-sys` on macOS; works unsigned).

### Lifecycle

```
┌──────────────┐  on_capture  ┌──────────────┐  on_complete  ┌──────────────┐
│  Clipboard   │ (optional) ▶ │  Pipeline    │ ──────────── ▶│  Notifier    │
│  event       │              │  filter → OCR│               │  + copy path │
└──────────────┘              │  → persist   │               └──────┬───────┘
                              └──────────────┘                      │
                                                                    ▼
                                    ┌────────────────────────────────────────┐
                                    │ macOS Notification Center              │
                                    │ Title: "textlog"                       │
                                    │ Body:  "Saved — 2026-04-14.md"         │
                                    │                                        │
                                    │ Side effect (copy_log_path_on_complete)│
                                    │   Clipboard ← "/Users/…/2026-04-14.md" │
                                    └────────────────────────────────────────┘
```

### Feedback-loop prevention

Writing the log path back to the clipboard would re-trigger the monitor. Two-layer suppression:

1. **Self-write token** — notifier records the `NSPasteboard.changeCount` it is about to create; the monitor skips the matching event.
2. **Path heuristic** — `monitoring.ignore_own_log_paths = true` causes the monitor to drop any capture whose content, when resolved as a filesystem path, falls inside `storage.log_dir`. Belt and braces.

---

## macOS Permissions

### Clipboard (macOS 15.4+ / 16)

Every programmatic `NSPasteboard.stringForType` / `dataForType` read triggers a system banner **"<app> pasted from <source>"** unless the user has granted always-allow in **System Settings → Privacy & Security → Paste from Other Apps**. The monitor mitigates by:

- Calling `NSPasteboard.types()` / `detect()` first — no banner, only metadata.
- Only reading content when the type list includes `public.utf8-plain-text`, `public.image`, or `public.file-url`.
- Requesting `NSPasteboard.accessBehavior = .always` at daemon start so the Privacy pane shows textlog with sensible defaults.

### Notifications

`notify-rust` → `mac-notification-sys` triggers macOS's notification permission dialog on first send. User approves once.

### `tl doctor` checks

- Clipboard access state
- Notification Center authorisation
- SQLite DB reachable + writable
- Log dir writable + `0600` permissions on config file
- LaunchAgent installed / loaded / last exit code
- MCP registration with Claude Code (`claude mcp list | grep textlog`)

### First-run UX

```
textlog needs two macOS permissions:

  1. Paste from Other Apps  — required to read clipboard
     Grant on first copy, or manually in System Settings.

  2. Notifications          — save/complete alerts
     Requested on first notification.

Register with Claude Code:
     claude mcp add textlog -- $(which tl) mcp

Starting monitor…
```

---

## Data Format

### SQLite schema (`~/textlog/index.db`)

```sql
CREATE TABLE captures (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              TEXT    NOT NULL,      -- ISO 8601 with TZ
    kind            TEXT    NOT NULL,      -- 'text' | 'image' | 'file'
    sha256          TEXT    NOT NULL,      -- content hash for dedup
    size_bytes      INTEGER NOT NULL,
    content         TEXT,                  -- text captures, or OCR'd text for images
    ocr_confidence  REAL,                  -- images only (0.0–1.0)
    source_app      TEXT,                  -- best-effort: the frontmost app
    source_url      TEXT,                  -- if pasteboard carried a URL
    md_path         TEXT                   -- path of the daily MD file where this row is mirrored
);

CREATE INDEX idx_captures_ts     ON captures(ts DESC);
CREATE INDEX idx_captures_sha256 ON captures(sha256);
CREATE INDEX idx_captures_kind   ON captures(kind);

CREATE VIRTUAL TABLE captures_fts USING fts5(
    content, content='captures', content_rowid='id'
);
```

Ring-buffer policy: on insert, `DELETE FROM captures WHERE id NOT IN (SELECT id FROM captures ORDER BY id DESC LIMIT storage.ring_buffer_size)`. Markdown archive is never trimmed.

### Markdown daily file (`~/textlog/logs/YYYY-MM-DD.md`)

Unchanged from v1 — YAML frontmatter + body. `ocr_text` field replaced by `content` (same value, single source of truth).

```markdown
---
timestamp: 2026-04-14T21:02:11+09:30
kind: text
sha256: 1a2b3c…
size_bytes: 142
source_app: "iTerm2"
---
thread 'main' panicked at 'index out of bounds: the len is 0 but the index is 0', src/main.rs:14:20
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
---
timestamp: 2026-04-14T20:58:03+09:30
kind: image
sha256: 9f3c…
size_bytes: 82431
ocr_confidence: 0.93
source_app: "Safari"
---
error: no space left on device (os error 28)
```

### Frontmatter schema

| Field | Type | Required | Description |
|---|---|---|---|
| `timestamp` | ISO 8601 | yes | Capture time |
| `kind` | string | yes | `text` \| `image` \| `file` |
| `sha256` | hex string | yes | Content hash |
| `size_bytes` | integer | yes | Bytes of original content |
| `ocr_confidence` | float 0-1 | images only | Vision framework mean confidence |
| `source_app` | string | no | Frontmost app at capture time |
| `source_url` | string | no | URL if pasteboard carried one |

---

## Technology Stack

### Core Dependencies (April 2026)

| Component | Crate | Version | Purpose |
|---|---|---|---|
| CLI | clap | 4.5 | Argument parsing |
| CLI | clap_complete | 4.5 | Shell completions |
| Async | tokio | 1.42 | Runtime |
| Serialization | serde | 1.0 | Config/JSON |
| Serialization | toml | 0.8 | TOML parsing |
| Serialization | serde_json | 1.0 | JSON-RPC bodies |
| Date/Time | chrono | 0.4 | Timestamps |
| Errors | anyhow | 1.0 | Propagation |
| Errors | thiserror | 2.0 | Error types |
| Logging | tracing | 0.1 | Internal logging |
| Logging | tracing-subscriber | 0.3 | Formatters |
| Dirs | dirs | 5.0 | Standard paths |
| Regex | regex | 1 | Privacy filters + search |
| SQLite | rusqlite | 0.31 | Ring buffer + FTS5 |
| Hash | sha2 | 0.10 | Content hashing for dedup |
| macOS FFI | objc2 | 0.6 | Objective-C runtime |
| macOS FFI | objc2-foundation | 0.3 | Foundation types |
| macOS FFI | objc2-app-kit | 0.3 | NSPasteboard, `detect()`, `accessBehavior` |
| macOS FFI | objc2-vision | 0.3 | `VNRecognizeTextRequest` for OCR |
| Notifications | notify-rust | 4 | Desktop notifications |
| MCP | rmcp | 0.2 | Rust MCP SDK (stdio transport) |

### Rust metadata

```toml
edition = "2021"
rust-version = "1.80"   # verify with `cargo msrv` once code exists
```

### Feature flags

| Flag | Default | Enables |
|---|---|---|
| `mcp` | on | MCP server (`tl mcp`) |
| `service` | on | LaunchAgent management |

No provider / cloud feature flags — none needed.

---

## Security & Privacy

### Threat model

- **Local-only.** No network egress except what Claude Code itself performs when you use it. textlog the daemon never opens a socket.
- **No secrets in config.** No API keys, no cloud endpoints. Config is a pure local-behavior file.
- **Content at rest** lives in SQLite + Markdown inside `~/textlog/`. Both are user-readable only (`0600`).

### Privacy filters

```rust
// Compiled from monitoring.ignore_patterns at daemon start
r"^sk-[A-Za-z0-9]{20,}"
r"^\w+_KEY\s*="
r"\b\d{4}[- ]?\d{4}[- ]?\d{4}[- ]?\d{4}\b"
r"(?i)password\s*[=:]\s*\S+"
```

### Data protection

1. Log directory + SQLite excluded from git by default.
2. No cloud transmission inside the daemon.
3. Config file permissions checked — warn if not `0600`.
4. Self-write suppression so our own clipboard writes do not double-log.
5. `textlog__clear_since` tool exposes programmatic privacy cut-off.

### Security checklist

- [ ] No hardcoded secrets
- [ ] All user input validated (regex compile at load, bounded lengths on MCP args)
- [ ] Config file permissions checked
- [ ] Sensitive patterns documented + tested
- [ ] No telemetry
- [ ] MCP input size caps enforced (`max_search_results_bytes`)

---

## Error Handling

### Clipboard permission not granted

```
$ tl start
⚠️  macOS has not granted clipboard access
   The first copy while monitoring will prompt the system dialog.
   Click "Always Allow" for textlog to continue silently.
```

### SQLite unavailable

```
$ tl start
❌ Cannot open ~/textlog/index.db: permission denied

To fix:
  chmod u+rwx ~/textlog
  rm ~/textlog/index.db && tl start      # rebuild (archive stays)
```

### MCP not registered

```
$ tl doctor
⚠️  textlog is not registered with Claude Code
   Fix: claude mcp add textlog -- $(which tl) mcp
```

### LaunchAgent plist conflict

```
$ tl install
❌ A different binary is already registered as com.textlog.agent
   Plist: ~/Library/LaunchAgents/com.textlog.agent.plist
   Points to: /usr/local/Cellar/.../tl
   To fix:  tl uninstall && tl install
```

### OCR failure

OCR errors never block a capture — the row is still stored with `content = ""`, `ocr_confidence = 0.0`, and a warning is surfaced once per session. `textlog__ocr_image` returns an explicit error for ad-hoc calls.

---

## Testing Strategy

### Unit tests
- Config parsing + round-trip + env overlay
- Privacy filter pattern matching
- Markdown frontmatter serialisation
- SQLite migrations + ring-buffer eviction
- SHA-256 dedup
- MCP request/response JSON shape

### Integration tests
- Apple Vision OCR on fixture images (covered by CI image assets)
- SQLite FTS5 search returns expected matches
- Clipboard → pipeline → storage end-to-end (using a mock `Pasteboard` trait)
- MCP server handshake, `tools/list`, `tools/call` via piped stdio

### E2E (manual, scripted)
1. `tl install && tl start`
2. Copy text — verify `textlog__get_recent(1)` returns it (via `echo '{...}' | tl mcp`)
3. Copy screenshot — verify OCR text in result
4. Trigger privacy filter — verify capture suppressed + notification
5. `tl doctor` — all green
6. `tl stop && tl uninstall` — cleanup

### Coverage target
- **Minimum**: 75% overall
- **Critical paths**: 90%+ (clipboard, storage, MCP server)

---

## Future Enhancements (Out of Scope for v2.0)

1. **Semantic search** over the Markdown archive (embedding index, `textlog__semantic_search`)
2. **Linux / Windows** clipboard backends (cross-platform)
3. **Screenshot segmentation** — crop and OCR only the relevant region
4. **MCP prompts** — expose pre-canned prompts like "explain this stack trace" as MCP `prompts/*`
5. **Export** — `tl export --format json|jsonl --since DATE`
6. **MCP resources** — expose historical archive as resource URIs
7. **CLI `tl ask`** using the user's active Claude Code session (via the Claude CLI's own API) — keeps the "no parallel LLM" rule
8. **Log rotation + archival**
9. **Web UI** for browsing the SQLite index

---

## Appendix: OCR Strategy

### Apple Vision Framework

- API: `VNRecognizeTextRequest` via `objc2-vision`.
- Modes: `.fast` (real-time) or `.accurate` (default).
- Languages: configurable via `ocr.languages`.
- Performance: ~30–80 ms per 1920×1080 screenshot on Apple Silicon.
- Cost: free, on-device, no network.

### Flow

1. Clipboard yields an image → pipeline creates an `NSImage` from the data.
2. If `image.max_dimension > ocr.image_max_dimension`, downscale (performance).
3. Submit `VNRecognizeTextRequest`; collect `VNRecognizedTextObservation` results.
4. Concatenate `topCandidates(1)` strings; compute mean confidence.
5. Store `{text, confidence}` in SQLite + Markdown row.
6. If `confidence < ocr.min_confidence`, still store but mark in notification.

No fallback: Apple Vision is the single OCR engine. If a user has a serious use case for a different engine, add it behind a feature flag in v2.1.

---

**Document Version**: 2.0
**Last Updated**: 2026-04-14 (pivoted to MCP server + Apple Vision OCR)
