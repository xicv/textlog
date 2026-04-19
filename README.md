# textlog

> A macOS clipboard + screenshot capture daemon that exposes its archive to
> Claude Code as a Model Context Protocol (MCP) server, with on-device OCR
> via Apple Vision Framework.

`textlog` runs quietly in the background, watches your clipboard and
screenshots, OCRs images with the same engine the macOS system uses, and
makes everything searchable by Claude Code through six `textlog__*` MCP
tools. Nothing leaves your machine. There is no LLM inside `textlog` —
Claude Code is the LLM, `textlog` is its eyes on your clipboard.

[![crates.io](https://img.shields.io/crates/v/textlog.svg)](https://crates.io/crates/textlog)
[![docs.rs](https://docs.rs/textlog/badge.svg)](https://docs.rs/textlog)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

---

## Table of contents

- [Why this exists](#why-this-exists)
- [Features](#features)
- [Quickstart](#quickstart)
- [Real scenarios](#real-scenarios)
- [Architecture](#architecture)
- [CLI reference](#cli-reference)
- [MCP tools](#mcp-tools)
- [Configuration](#configuration)
- [Tuning recipes](#tuning-recipes)
- [Privacy](#privacy)
- [macOS permissions](#macos-permissions)
- [FAQ / troubleshooting](#faq--troubleshooting)
- [Comparison with alternatives](#comparison-with-alternatives)
- [Development](#development)
- [License](#license)

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
  (`"accurate"` | `"fast"`) and a configurable language list.
- **SQLite + FTS5 ring buffer.** Bounded full-text index (default 1000
  captures), automatically trimmed. The daily Markdown archive is
  **never** trimmed — the SQL index is for query speed; the MD files
  are the durable record.
- **SHA-256 dedup at query time.** `get_recent` and `list_today` collapse
  duplicates by content hash; `search` returns every match but marks
  later occurrences with `duplicate_of`, so Claude sees each unique
  snippet exactly once per call.
- **`md_path` on every result.** Every capture row in every MCP response
  carries the absolute path of the daily Markdown file it was mirrored
  into, so Claude can `Read` the whole day's context as a single file
  attachment without any clipboard round-trip.
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

### What's new in v0.1.1

- `md_path` is now included on every `CaptureSummary`. This makes
  scenario 10 ("daily-archive paste trick") work without
  `notifications.copy_log_path_on_complete` — recommended for users
  whose clipboard manager (Raycast / Maccy / Paste / Alfred) cascades
  on the path-back-to-clipboard write.

---

## Quickstart

```bash
# Build and install (requires Rust 1.80+ and macOS).
cargo install textlog                   # from crates.io
# or
cargo install --path .                  # from a local clone

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

If you'd rather run only when you ask, skip `tl install` and run
`tl start --foreground` in a terminal tab — the daemon prints to
stdout and stops on Ctrl-C.

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

### 10. Daily-archive paste trick (v0.1.1+ — no clipboard side-effect)
Every capture row in every MCP response now carries `md_path`. So:
> *"Read the daily clipboard log and summarise my morning."*

Claude calls `textlog__get_recent { n: 1 }`, sees
`md_path: "/Users/you/textlog/logs/2026-04-17.md"` in the response,
then calls its built-in `Read` tool on that path. Claude receives the
entire day's chronological transcript as a single attachment.

In v0.1.0 the same workflow needed `notifications.copy_log_path_on_complete`,
which copied the path to the clipboard after each capture and could
cascade with some clipboard managers. v0.1.1 makes that side-effect
optional — you can turn it off and Claude still finds the file.

### 11. Code review prep
Reviewing a PR? Click through every changed file in your IDE, copy the
unified diff for each, then:
> *"For each clipboard entry from the last 10 minutes that looks like a
> diff, list the function-level changes."*

### 12. Slack/Linear ticket triage
Copy the full ticket body from Linear, copy the relevant log line from
your terminal, then:
> *"Cross-reference my last two clipboard entries and tell me whether
> the log proves the bug from the ticket."*

### 13. Comparing two API responses
Copy yesterday's response, copy today's response, then:
> *"What changed between my last two clipboard entries (both JSON)?"*

`textlog__get_recent { n: 2 }` → Claude diffs two JSON blobs without
you opening a diff tool.

### 14. "Save this for later" without manual notes
You're in the middle of fixing bug A and notice symptom B. Cmd+C the
relevant piece of evidence for B and keep working on A. Tomorrow:
> *"Find clipboard entries from yesterday that look like error messages
> I haven't followed up on."*

### 15. AI coding pair-prog feedback loop
Claude writes a function. You copy the test failure. Claude reads
`textlog__get_recent`, sees its own previously-suggested code in
context with the new failure, iterates without you re-pasting.

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
|                     (+ optional clipboard write- |
|                      back of the daily MD path)  |
+--------------------------------------------------+
```

Two long-running tasks joined via `tokio::select!`:

1. **Monitor loop** — polls `NSPasteboard.changeCount` directly on
   the async task (it's a microsecond i64 read, no `spawn_blocking`
   needed). Only falls through to `clipboard::poll_once` (FFI content
   read) via `spawn_blocking` when the counter actually advances.
   Exponential idle backoff: 500 ms active, doubling to a 2 s ceiling
   after 20 unchanged ticks; any real change snaps back to active.
   Pushes `ClipboardEvent` into a bounded `mpsc(16)` channel with
   drop-on-full backpressure.
2. **Consumer** — drains the channel, runs filter → OCR → SHA-256 →
   `Storage::insert` → notifier, all SQLite work `spawn_blocking`'d so
   concurrent MCP queries are never blocked by a slow disk.

### Why these specific choices

- **Polling, not event-driven.** macOS `NSPasteboard` has no public
  notification API for change events. The default 500 ms interval is
  perceptually instant; combined with exponential idle backoff (up to
  2 s) and a direct `changeCount` fast-path (no `spawn_blocking` when
  unchanged), CPU at idle is effectively zero — only a content read
  happens when the counter actually advances.
- **SQLite + FTS5, not a fancier search engine.** FTS5 is built into
  SQLite, supports `MATCH 'foo'` queries with prefix wildcards, and
  ships zero extra binaries. Latency for a single-keyword search over
  10k rows is sub-millisecond on Apple Silicon.
- **Ring buffer + permanent MD archive.** The bounded SQL index is
  what makes `tl logs search` fast forever; the MD archive is what
  makes "what did I copy three weeks ago" still answerable. Neither
  alone is enough.
- **Apple Vision, not Tesseract or a cloud OCR.** Vision ships with
  macOS, requires no setup, supports 30+ languages, and runs on the
  Neural Engine when available. A typical screenshot OCRs in <100 ms
  with `recognition_level = "accurate"`.
- **rmcp 1.4 stdio.** Stdio JSON-RPC is the only MCP transport Claude
  Code's `claude mcp add` supports out of the box. No HTTP server, no
  port collisions.

---

## CLI reference

```
tl mcp                          Run MCP stdio server (Claude Code spawns this)
tl version | -v | --version     Print version (subcommand or short flag)
tl update                       Self-update via `cargo install textlog --force`
tl config show                  Print effective config as TOML
tl config path                  Print config file path
tl config reset                 Overwrite config with v2.0 defaults

tl logs today                   List today's captures (one line each)
tl logs search <QUERY> [--limit N]   FTS5 search; canonical row only
tl logs path                    Print log directory

tl doctor                       Run 8 health checks; non-zero exit on FAIL
tl perf [--duration SECS]       Sample daemon CPU/RSS via `ps`
       [--interval-ms MS]        (default 10 s @ 1 s)

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
| `textlog__get_recent` | `n` (default 5), `kind?` (`text`\|`image`\|`any`) | `{ captures: [{ id, ts, kind, sha256, size_bytes, text, md_path, source_app?, source_url?, ocr_confidence? }] }` | Latest N captures, deduped by sha256 |
| `textlog__list_today` | `kind?` | same shape as `get_recent` | Everything from today (UTC) |
| `textlog__search` | `query`, `limit` (default 20), `since?` ISO 8601 | `{ hits: [{ capture, duplicate_of? }] }` | FTS5 search; later occurrences marked |
| `textlog__ocr_latest` | none | `{ text, confidence, captured_at }` | Cached OCR text from the last image |
| `textlog__ocr_image` | `path` (absolute) | `{ text, confidence, block_count }` | Ad-hoc OCR of any file |
| `textlog__clear_since` | `ts` ISO 8601 | `{ deleted_count }` | Privacy cut-off (SQLite only; MD untouched) |

All input/output schemas are published via MCP `tools/list`.
`md_path` was added in v0.1.1 — it's the absolute path of the daily
Markdown file the row was mirrored into, so Claude can `Read` the
entire day's context without any clipboard round-trip.

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
| `monitoring.poll_interval_ms` | `500` | Active-rate ceiling; idle backoff slows to a 2 s cap automatically. Lower = snappier; higher = even less CPU |
| `monitoring.min_length` | `10` | Drop tiny copies (tab-switching noise). Lower to 1 if you want every clipboard transition recorded |
| `monitoring.ignore_patterns` | API keys, CC numbers, passwords | Add your own regexes — anything matched is silently dropped |
| `notifications.copy_log_path_on_complete` | `true` | If your clipboard manager cascades, set `false`. Claude still finds the path via `md_path` in MCP responses (v0.1.1+) |
| `notifications.on_capture` | `false` | A toast for every clipboard event — usually too noisy |
| `storage.ring_buffer_size` | `1000` | SQLite cap; MD archive is never trimmed |
| `storage.log_dir` | `~/textlog/logs` | Where daily MD files live |
| `ocr.recognition_level` | `"accurate"` | `"fast"` if Vision is too slow on your hardware |
| `ocr.languages` | `["en-US"]` | Add more — Vision's polyglot is excellent |
| `ocr.min_confidence` | `0.4` | Lower to keep blurrier OCR text; higher to drop low-confidence results |

After editing: `tl uninstall && tl install` so the LaunchAgent re-reads
the (cached) program args. (For `tl start --foreground`, just Ctrl-C
and re-run.)

---

## Tuning recipes

### Minimal-noise mode
For users who only want substantive captures and no clipboard side-effects.

```toml
[monitoring]
min_length = 50                     # ignore tab-switch noise

[notifications]
enabled = false                     # no toasts at all
copy_log_path_on_complete = false   # no clipboard write-back
```

### Maximum-recall mode
For users who want every clipboard transition recorded, even single chars.

```toml
[monitoring]
min_length = 1
poll_interval_ms = 100              # 4× faster polling

[storage]
ring_buffer_size = 10000            # ~10× the default; SQLite handles this fine
```

### Multilingual capture
Vision supports 30+ languages. List them in priority order — earlier
languages bias the recogniser when text is ambiguous.

```toml
[ocr]
languages = ["en-US", "ja-JP", "zh-Hans", "fr-FR", "de-DE"]
recognition_level = "accurate"
```

### Strict privacy
Goes beyond the defaults — drops anything that looks remotely like a
secret or a finance number.

```toml
[monitoring]
ignore_patterns = [
    "^sk-[A-Za-z0-9]{20,}",
    "^\\w+_KEY\\s*=",
    "^\\w+_TOKEN\\s*=",
    "^\\w+_SECRET\\s*=",
    "Bearer\\s+[A-Za-z0-9._-]{20,}",
    "\\b\\d{4}[- ]?\\d{4}[- ]?\\d{4}[- ]?\\d{4}\\b",   # credit cards
    "\\b\\d{3}[- ]?\\d{2}[- ]?\\d{4}\\b",                # US SSN
    "(?i)password\\s*[=:]\\s*\\S+",
    "-----BEGIN [A-Z ]*PRIVATE KEY-----",
]

[privacy]
filter_enabled = true
log_sensitive = false
show_filter_notification = true     # always notify on a hit
```

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
- **`textlog__clear_since`** lets you (or Claude) wipe SQLite rows at
  or after a timestamp. The Markdown archive is intentionally untouched
  — delete those files yourself if you want them gone.
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

## FAQ / troubleshooting

### `tl doctor` first, every time

```bash
tl doctor                          # always start here
cat ~/textlog/logs/stdout.log      # LaunchAgent stdout
cat ~/textlog/logs/stderr.log      # LaunchAgent stderr
```

### Why isn't `"hi?"` showing up in `tl logs today`?

Default `monitoring.min_length = 10`. Anything shorter is dropped before
storage. Either accept it as noise filtering, or:

```bash
sed -i '' 's/^min_length = .*/min_length = 1/' ~/textlog/config.toml
```

then restart the daemon.

### Why do I see the same content multiple times in the MD file?

`Storage::insert` records every clipboard *transition*. If your clipboard
manager (Raycast / Maccy / Paste / Alfred) re-asserts content, your terminal
has copy-on-select enabled, or you re-copy the same text by hand, each
write bumps `NSPasteboard.changeCount` and the daemon sees a new event.

These duplicates are **never visible to Claude**: query-time dedup
collapses them by `sha256`. The MD file is the durable per-event audit
trail; the SQL queries are what Claude actually consumes.

To reduce the MD-file duplication directly:
- Disable `notifications.copy_log_path_on_complete` (v0.1.1+ — no
  longer needed for path discovery).
- Disable copy-on-select in your terminal app's preferences.

### Claude can't find my MCP server

```bash
claude mcp list                    # textlog should show ✓ Connected
```

If `✗ Failed to connect`:

```bash
# Hand-drive an initialize request to see the actual error:
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"manual","version":"0"}}}' | tl mcp
```

If you see a JSON response, the server is fine — the issue is in
Claude Code's registration. Try `claude mcp remove textlog` then re-add.

### My screenshot OCR returns no text

Check `ocr.min_confidence` (default `0.4`). On low-resolution screenshots
or blurry text, Vision's confidence may sit below the threshold. Lower
it to `0.2` to be more permissive, or set
`ocr.recognition_level = "accurate"` if it's currently `"fast"`.

### The daemon stopped after I edited config

LaunchAgent caches `ProgramArguments`, not the config-file path or
contents. The config is re-read on each `tl start`, so:

```bash
tl uninstall && tl install        # rewrites the plist + restarts cleanly
```

For `tl start --foreground`, just Ctrl-C and re-run.

### Nuclear reset

```bash
tl uninstall
rm -rf ~/textlog                  # config + db + MD archive all gone
tl doctor                         # confirms clean slate
```

### Pasteboard permission denied

System Settings → Privacy & Security → Pasteboard → enable `tl`.

### Notifications muted

System Settings → Notifications → `tl` → enable. (Or set
`notifications.enabled = false` to never bother.)

---

## Comparison with alternatives

|  | textlog | Raycast Clipboard History | Maccy / Paste | Rewind.ai |
|---|---|---|---|---|
| Background clipboard capture | yes | yes | yes | yes |
| Searchable history | yes (FTS5) | yes (UI only) | yes (UI only) | yes |
| Image OCR at capture time | **yes (Apple Vision)** | no | no | yes (cloud + local mix) |
| Exposes to LLM via MCP | **yes** | no | no | no |
| Local-only (zero network) | **yes** | yes | yes | no (cloud component) |
| Per-day Markdown archive | **yes** | no | no | no |
| Privacy regex filter at capture | **yes** | no | partial | no |
| Open source | **yes (MIT)** | proprietary | proprietary | proprietary |
| Built specifically for Claude Code | **yes** | no | no | no |
| Standalone clipboard UI | **no** (intentional) | yes | yes | yes |

`textlog` is not a clipboard manager. It's an LLM-facing clipboard
backend. Use Raycast or Maccy for browsing your history visually; use
`textlog` for letting Claude reason over it.

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
    perf.rs             tl perf CPU/RSS sampling
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

Contributions welcome — open an issue or PR at
[github.com/xicv/textlog](https://github.com/xicv/textlog).

---

## License

MIT. See `LICENSE`.
