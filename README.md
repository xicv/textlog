# textlog

> A macOS clipboard + screenshot capture daemon that exposes its archive to
> Claude Code as a Model Context Protocol (MCP) server, with on-device OCR
> via Apple Vision Framework.

`textlog` runs quietly in the background, watches your clipboard and
screenshots, OCRs images with the same engine the macOS system uses, and
makes everything searchable by Claude Code through six `textlog__*` MCP tools.
Nothing leaves your machine. There is no LLM inside `textlog` — Claude Code
is the LLM, `textlog` is its eyes on your clipboard.

---

## Why this exists

Pasting context into Claude Code one selection at a time is the default, and
it works. But it has three failure modes:

1. **You paste the wrong slice.** A 400-line stack trace gets trimmed to
   the boring middle, the actual panic line is in the 20 lines you missed.
2. **You can't paste at all.** The bug is in a screenshot from your designer,
   a render of a PDF, or an image in a Notion doc. Claude can read images,
   but you spend tokens uploading the raw PNG when you only needed the text.
3. **You forgot what you copied.** You know you saw the right error message
   ten minutes ago. It was on your clipboard. Now it's not.

`textlog` fixes all three. Every clipboard transition lands in a SQLite
ring buffer (1000 captures by default) **and** a permanent daily Markdown
file. Images are OCR'd at capture time, so by the time you ask Claude
"what was in that screenshot?" the answer is already a plain string in
the index — no image upload, no token bloat.

It is built specifically for the Claude Code workflow: the daemon's
job is to be a high-recall, low-latency clipboard backend for an LLM
that lives next door, not a standalone clipboard manager with its own
UI. The MCP server is the entire user-facing surface.

---

## Features

- **MCP server with 6 tools** — `textlog__get_recent`, `__search`,
  `__list_today`, `__ocr_latest`, `__ocr_image`, `__clear_since`.
  Registers in one command: `claude mcp add textlog -- tl mcp`.
- **On-device OCR** via `VNRecognizeTextRequest` (Apple Vision). No cloud
  call, no API key, no rate limit. Honours `recognition_level`
  ("accurate" | "fast") and a configurable language list.
- **SQLite + FTS5 ring buffer.** Bounded full-text index (default 1000
  captures), automatically trimmed. The daily Markdown archive is
  **never** trimmed — the SQL index is for query speed; the MD files
  are the durable record.
- **SHA-256 dedup at query time.** `get_recent` and `list_today` collapse
  duplicates by content hash; `search` returns every match but marks
  later occurrences with `duplicate_of`, so Claude sees each unique
  snippet exactly once per call.
- **Privacy filter.** A `RegexSet` is compiled once at startup from
  `monitoring.ignore_patterns`. Defaults catch OpenAI-style API keys,
  `*_KEY=` env-var assignments, 16-digit credit-card numbers, and
  `password = …` lines. Hits are dropped before they reach storage and
  optionally surface a discreet macOS notification.
- **Self-write skip.** When the daemon writes the daily-MD path back to
  your clipboard (`copy_log_path_on_complete`), it publishes the
  resulting `NSPasteboard.changeCount` into a shared atomic so the next
  poll skips its own write — no infinite recursion.
- **LaunchAgent integration.** `tl install` writes a plist and runs
  `launchctl bootstrap gui/$UID` so the daemon survives reboots.
  `tl uninstall` does the inverse.
- **`tl doctor`** runs eight health checks across config, storage,
  permissions, LaunchAgent state, MCP registration, and a live Vision
  smoke test, then exits non-zero if anything is broken.

---

## Quickstart

```bash
# Build and install (requires Rust 1.80+ and macOS).
cargo install --path .

# First-run health check — should print 4 PASS, 4 WARN, 0 FAIL.
tl doctor

# Optional: register as a LaunchAgent so it auto-starts on login.
tl install

# Register as an MCP server in Claude Code.
claude mcp add textlog -- tl mcp

# Confirm.
claude mcp list
```

That's it. Open any Claude Code session and ask "what's in my recent
clipboard?" — Claude calls `textlog__get_recent` and reads the answer.

---

## Real scenarios

### 1. The "what just blew up" loop
Terminal spits out a 40-line stack trace. Instead of selecting carefully,
`Cmd+A; Cmd+C` the whole pane and ask Claude:
> *"Look at my last clipboard entry — what's the root cause and what file
> should I open?"*

Claude calls `textlog__get_recent { n: 1 }` and points you at the line.

### 2. Screenshot a UI bug, get a CSS fix back
A designer slacks you a screenshot of a misaligned button. `Cmd+Shift+Ctrl+4`
to copy the screenshot to clipboard, then:
> *"OCR the last image I copied and suggest CSS fixes for what it shows."*

`textlog__ocr_latest` returns the OCR'd text from the screenshot — no
image upload, no token waste.

### 3. Compiler error from a different terminal tab
A 300-line Rust error spans two screens. You don't want to scroll-select.
> *"Search my clipboard log for the most recent 'cannot find function'
> error and explain it."*

`textlog__search { query: "cannot find function", limit: 1 }`.

### 4. Recovering yesterday's API response
Postman returned a JSON blob you reasoned about, then you closed the tab.
> *"Find that Stripe webhook payload from yesterday."*

`textlog__search { query: "evt_", since: "2026-04-16T00:00:00Z" }`.

### 5. Dense docs you OCR'd from a PDF
PDFs with broken text selection are a known pain. Screenshot a code
block from one and:
> *"OCR this and convert the Swift to Rust."*

### 6. "Why did this work yesterday?"
You're debugging a regression. You copied the working version of a
function six hours ago.
> *"Search clipboard for `fn calculate_total`, show the most recent
> three unique versions."*

FTS5 search + sha256 dedup means Claude only sees distinct revisions —
not 40 copies of the same paste.

### 7. Pre-commit context dump
You've been scribbling into your clipboard for an hour: error messages,
command outputs, `dbg!` results.
> *"Summarise my clipboard from the last hour into a draft commit message."*

`textlog__list_today` hands Claude the entire investigation trail.

### 8. Privacy-respecting paste
You accidentally `Cmd+C`'d `OPENAI_API_KEY=sk-proj-…`. The default
filter dropped it before storage. A discreet notification confirms:
*"textlog dropped a sensitive clipboard entry"*. The key was never
written to disk.

### 9. Cross-session memory
Three days later, after a reboot:
> *"Did I look at the Stripe webhook docs this week?"*

`textlog__search { query: "webhook", since: "2026-04-14T00:00:00Z" }`.

### 10. Daily-archive paste trick
Default-on. Every capture ends with the daily-MD file path on your
clipboard.
- Copy something interesting
- Hit `Cmd+V` in Claude — you get the *path* to that day's file
- Tell Claude *"read that file"* — Claude has the entire day's
  context as a single attachment

The self-write skip makes this loop safe: the daemon ignores its own
write of the path, so you don't get a recursive capture.

---

## Architecture

```
                    +-------------------+
                    |   Claude Code     |
                    +---------+---------+
                              | stdio JSON-RPC 2.0
                              v
+--------------------------------------------------------+
|                     tl mcp (MCP server)                |
|   textlog__get_recent / __search / __list_today        |
|   textlog__ocr_latest / __ocr_image / __clear_since    |
+----------------------+---------------------------------+
                       |
                       v
        +--------------+----------------+
        |   src/storage/sqlite.rs       |   <-- SQLite + FTS5 ring buffer
        |   src/storage/markdown.rs     |   <-- daily MD archive (durable)
        +---------------+---------------+
                        ^
                        |  insert(CaptureRow)
                        |
+-----------------------+--------------------------+
|             tl start (pipeline)                  |
|                                                  |
|  clipboard::poll_once  -->  PrivacyFilter        |
|        ^                          |              |
|        |                          v              |
|  NSPasteboard      ocr::ocr_image (Vision)       |
|        ^                          |              |
|        |                          v              |
|        +----------- notifier::notify_complete    |
|                     (+ clipboard::write_text     |
|                      for log-path copy-back)     |
+--------------------------------------------------+
```

Two long-running tasks joined via `tokio::select!`:

1. **Monitor loop** — `tokio::time::interval(poll_interval_ms)`,
   `clipboard::poll_once` via `spawn_blocking`, pushes
   `ClipboardEvent` into a bounded `mpsc(16)` channel with
   drop-on-full backpressure.
2. **Consumer** — drains the channel, runs filter → OCR → SHA-256 →
   `Storage::insert` → notifier, all SQLite work `spawn_blocking`'d so
   concurrent MCP queries are never blocked by a slow disk.

---

## CLI reference

```
tl mcp                          Run MCP stdio server (Claude Code spawns this)
tl version                      Print version
tl config show                  Print effective config as TOML
tl config path                  Print config file path
tl config reset                 Overwrite config with v2.0 defaults

tl logs today                   List today's captures (one line each)
tl logs search <QUERY> [--limit N]   FTS5 search; canonical row only
tl logs path                    Print log directory

tl doctor                       Run 8 health checks; non-zero exit on FAIL

tl install                      Install LaunchAgent + bootstrap
tl uninstall                    Bootout + remove plist
tl start                        launchctl kickstart (background)
tl start --foreground           Run pipeline inline (Ctrl-C to stop)
tl stop                         launchctl kill SIGTERM
tl status                       Loaded? PID? Last exit?
```

Global flag: `--config-dir <PATH>` (env: `TEXTLOG_CONFIG_DIR`) overrides
`~/textlog/`.

---

## MCP tools

| Tool | Args | Returns | Purpose |
|---|---|---|---|
| `textlog__get_recent` | `n` (default 5), `kind?` (`text`\|`image`\|`any`) | `{ captures: [...] }` | Latest N captures, deduped by sha256 |
| `textlog__list_today` | `kind?` | `{ captures: [...] }` | Everything from today (UTC) |
| `textlog__search` | `query`, `limit` (default 20), `since?` ISO 8601 | `{ hits: [{ capture, duplicate_of? }] }` | FTS5 search; later occurrences marked |
| `textlog__ocr_latest` | none | `{ text, confidence, captured_at }` | Cached OCR text from the last image |
| `textlog__ocr_image` | `path` (absolute) | `{ text, confidence, block_count }` | Ad-hoc OCR of any file |
| `textlog__clear_since` | `ts` ISO 8601 | `{ deleted_count }` | Privacy cut-off (SQLite only; MD untouched) |

All input/output schemas are published via MCP `tools/list`.

---

## Configuration

Default location: `~/textlog/config.toml`. Override with `TEXTLOG_CONFIG_DIR`.

Reset to defaults at any time:

```bash
tl config reset
$EDITOR $(tl config path)
```

Keys you'll most likely want to touch:

| key | default | why touch it |
|---|---|---|
| `monitoring.poll_interval_ms` | `250` | Lower = snappier; higher = less CPU |
| `monitoring.min_length` | `10` | Drop tiny copies (tab-switching noise) |
| `monitoring.ignore_patterns` | API keys, CC numbers, passwords | Add your own regexes — anything matched is silently dropped |
| `notifications.copy_log_path_on_complete` | `true` | Path-back-to-clipboard trick (scenario 10) |
| `storage.ring_buffer_size` | `1000` | SQLite cap; MD archive is never trimmed |
| `storage.log_dir` | `~/textlog/logs` | Where daily MD files live |
| `ocr.recognition_level` | `"accurate"` | `"fast"` if Vision is too slow on your hardware |
| `ocr.languages` | `["en-US"]` | Add more — Vision's polyglot is excellent |

After editing: `tl uninstall && tl install` so the LaunchAgent re-reads
the (cached) program args.

---

## Privacy

- **Nothing leaves your machine.** No telemetry, no auto-updates, no
  remote calls of any kind. The only outbound process is `claude mcp`
  spawning `tl mcp` as a local subprocess.
- **Privacy filter** drops sensitive content (API keys, credit cards,
  passwords) before it reaches storage. See
  `monitoring.ignore_patterns` for the default regex set; add your
  own. `privacy.show_filter_notification` toggles the discreet macOS
  notification on filter hits.
- **`tl logs clear-since`** exists via the MCP `textlog__clear_since`
  tool: it deletes SQLite rows at or after a timestamp. The Markdown
  archive is intentionally untouched — delete those files yourself if
  you want them gone.
- **Config file mode `0600`** is checked by `tl doctor`; warn-level if
  loose. The default config contains no secrets but you may add API
  keys (e.g. for future provider-aware features) so the warning stays.

---

## macOS permissions

On first run, macOS will prompt for:

- **Pasteboard access** (since macOS 15.4) — required.
- **Notifications** — optional, only if you have any
  `notifications.*` flag set to `true`.

`tl doctor` reports the state of both. Pasteboard denial is a hard
fail; notifications denial is informational.

---

## Troubleshooting

```bash
tl doctor                          # Always start here.

cat ~/textlog/logs/stdout.log      # LaunchAgent stdout
cat ~/textlog/logs/stderr.log      # LaunchAgent stderr

tl uninstall && rm -rf ~/textlog && tl doctor   # Nuclear reset

# Run the daemon visibly:
tl uninstall && tl start --foreground
```

If `claude mcp list` shows `✗ Failed to connect`:

```bash
# Hand-drive an initialize request to see the actual error:
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"manual","version":"0"}}}' | tl mcp
```

---

## Development

```bash
cargo test --bin tl                # 168 tests pass
cargo test --bin tl -- --ignored   # +5 live tests (NSPasteboard, Vision, notify-rust)
cargo clippy --bin tl --all-features --tests
```

Source layout:

```
src/
  main.rs               Entry point — clap parse + dispatch
  cli/
    args.rs             clap derive structs
    commands.rs         Per-command handlers
    doctor.rs           tl doctor health checks
  config/               TOML schema + load/save + env overlay
  error.rs              Top-level Error enum + Result alias
  filters.rs            Privacy filter (RegexSet)
  storage/
    mod.rs              Kind, CaptureRow, SearchHit, hex helpers
    sqlite.rs           rusqlite + FTS5 ring buffer
    markdown.rs         Daily MD archive writer
  ocr.rs                Apple Vision wrapper (objc2-vision)
  clipboard.rs          NSPasteboard read/write + ClipboardWriter trait
  notifier.rs           notify-rust + Notifier trait + CountingNotifier
  pipeline.rs           Pipeline::process_event + run loop
  mcp/
    mod.rs              run_stdio entry point
    tools.rs            McpServer + 6 #[tool] handlers
    schema.rs           Argument + result types (JsonSchema)
  service/
    mod.rs              install/uninstall/start/stop/status
    plist.rs            com.textlog.agent.plist generator
    launchctl.rs        LaunchctlRunner trait + System/Recording impls
  macos_perm.rs         Best-effort permission probes
```

Specs live in `docs/superpowers/specs/` and the implementation plan
in `docs/superpowers/plans/`.

---

## License

MIT. See `LICENSE`.
