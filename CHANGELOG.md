# Changelog

All notable changes to textlog are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.6] - 2026-04-19

### Changed

- **Breaking default.** `notifications.copy_log_path_on_complete` now
  defaults to `false`. Previously the daemon wrote the daily-MD path
  back to the clipboard after every capture, which cascaded in
  clipboard managers and surprised users who only expected textlog to
  *read* the clipboard. Claude already discovers the daily-MD via
  `md_path` in MCP responses since v0.1.1, so the write-back hasn't
  been necessary for a while.
- Existing configs are not rewritten. If you have
  `copy_log_path_on_complete = true` in `~/textlog/config.toml` and
  want the new behavior, set it to `false` (or delete the line).

## [0.1.5] - 2026-04-19

### Performance

- Move the `NSPasteboard.changeCount` check out of `spawn_blocking` —
  it's a microsecond i64 property read, not a blocking call. The
  blocking-pool handoff was paying a task alloc + context switch every
  tick for nothing.
- Only enter `spawn_blocking` when the counter actually advances and
  string / PNG content needs to be read.
- Exponential idle backoff: active rate = `poll_interval_ms`, doubles
  to a 2 s ceiling after 20 unchanged ticks; any real change snaps
  back to active.
- Default `poll_interval_ms` raised from 250 to 500 ms (matches
  Maccy). Idle wakeups drop by roughly an order of magnitude.

### Added

- `tl perf` command: samples the running daemon's CPU% and RSS via
  `ps` and reports min/avg/max, with config-driven context (poll
  interval, backoff ceiling) and a verdict line. Flags:
  `--duration <secs>` (default 10), `--interval-ms <ms>` (default
  1000).

### Docs

- README notes the launchd respawn-throttle workaround after a binary
  upgrade (`tl uninstall && tl install` clears the throttle).

## [0.1.4] - 2026-04-17

### Fixed

- MCP stdio deadlock: no longer hold `std::io::stdout().lock()`
  across `run_mcp` dispatch. The rmcp stdio transport writes from a
  `spawn_blocking` worker thread that needs the same reentrant lock,
  which caused `initialize` to hang silently. Each subcommand now
  locks stdout on its own.

## [0.1.3] - 2026-04-17

### Added

- Lowercase `-v` short flag alongside `-V` / `--version` / `tl
  version` — all four paths resolve to the same handler.
- `tl update` command: self-updater that shells out to `cargo install
  textlog --force` and prints the `tl uninstall && tl install` hint
  to re-bootstrap the LaunchAgent.

## [0.1.2] - 2026-04-17

### Added

- Storage benchmark (`bench_storage_at_scale`): inserts 10k rows,
  measures FTS5 search latency and daily MD archive size to set a
  performance baseline.

### Docs

- README expanded with real scenarios, full MCP tool reference,
  tuning recipes, and tilde-path policy.

## [0.1.1] - 2026-04-17

### Added

- `md_path` field on every `CaptureSummary` so Claude can cite the
  daily MD archive directly. Makes the daily-archive paste trick work
  without `notifications.copy_log_path_on_complete` — recommended for
  users whose clipboard manager (Raycast, Maccy, Paste, Alfred)
  cascades on the path-back-to-clipboard write.

## [0.1.0] - 2026-04-17

### Added

- Initial public release on crates.io.
- NSPasteboard polling pipeline with privacy filter and per-capture
  SHA-256 dedup.
- Apple Vision OCR (`VNRecognizeTextRequest`) for clipboard images
  and ad-hoc files, with `accurate` / `fast` recognition levels and
  configurable language + confidence thresholds.
- SQLite FTS5 ring buffer (bounded query index) plus a permanent
  daily Markdown archive under `~/textlog/logs`.
- MCP stdio server exposing six tools: `textlog__get_recent`,
  `textlog__list_today`, `textlog__search`, `textlog__ocr_latest`,
  `textlog__ocr_image`, `textlog__clear_since`.
- `tl doctor` with eight health checks: config file, log dir, SQLite
  + FTS5, pasteboard access, notifications, LaunchAgent, MCP
  registration, Vision smoke test.
- LaunchAgent lifecycle: `tl install` / `uninstall` / `start` /
  `stop` / `status`.

[Unreleased]: https://github.com/xicv/textlog/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/xicv/textlog/releases/tag/v0.1.5
[0.1.4]: https://github.com/xicv/textlog/releases/tag/v0.1.4
[0.1.3]: https://github.com/xicv/textlog/releases/tag/v0.1.3
[0.1.2]: https://github.com/xicv/textlog/releases/tag/v0.1.2
[0.1.1]: https://github.com/xicv/textlog/releases/tag/v0.1.1
[0.1.0]: https://github.com/xicv/textlog/releases/tag/v0.1.0
