//! Subcommand handlers. Each fn takes the loaded `Config` and the
//! relevant args, and writes its output through a `Write` sink so the
//! handler is unit-testable without capturing stdout.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::sync::atomic::AtomicI64;

use crate::clipboard::SystemClipboardWriter;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::notifier::SystemNotifier;
use crate::pipeline::Pipeline;
use crate::service::{self, ServiceStatus, SystemLaunchctl};
use crate::storage::{expand_tilde, hex_lower, CaptureRow, Kind, Storage};

use super::args::{Cli, Command, ConfigCmd, LogsCmd};

/// Top-level dispatch — async because `mcp` runs the rmcp server.
pub async fn dispatch(cli: Cli) -> Result<()> {
    let cfg_path = resolve_config_path(cli.config_dir.as_deref())?;
    let cfg = crate::config::load_or_init(&cfg_path)?;
    let mut stdout = std::io::stdout().lock();

    match cli.command {
        Command::Mcp => run_mcp(&cfg).await,
        Command::Version => print_version(&mut stdout),
        Command::Config { cmd } => run_config(cmd, &cfg, &cfg_path, &mut stdout),
        Command::Logs { cmd } => run_logs(cmd, &cfg, &mut stdout),
        Command::Doctor => print_unimplemented("doctor", &mut stdout),
        Command::Install => run_install(&cfg, &mut stdout),
        Command::Uninstall => run_uninstall(&mut stdout),
        Command::Start { foreground } => {
            if foreground {
                drop(stdout);
                run_start_foreground(&cfg).await
            } else {
                run_start(&mut stdout)
            }
        }
        Command::Stop => run_stop(&mut stdout),
        Command::Status => run_status(&mut stdout),
    }
}

fn run_install<W: Write>(cfg: &Config, out: &mut W) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Launchctl(format!("could not resolve current_exe: {e}")))?;
    let log_dir = expand_tilde(&cfg.storage.log_dir);
    std::fs::create_dir_all(&log_dir)?;
    let runner = SystemLaunchctl;
    let plist_path = service::install(&runner, &exe, &log_dir)?;
    writeln!(
        out,
        "installed LaunchAgent at {} (program: {})",
        plist_path.display(),
        exe.display(),
    )?;
    Ok(())
}

fn run_uninstall<W: Write>(out: &mut W) -> Result<()> {
    let runner = SystemLaunchctl;
    service::uninstall(&runner)?;
    writeln!(out, "uninstalled LaunchAgent com.textlog.agent")?;
    Ok(())
}

fn run_start<W: Write>(out: &mut W) -> Result<()> {
    service::start(&SystemLaunchctl)?;
    writeln!(out, "kickstarted com.textlog.agent")?;
    Ok(())
}

fn run_stop<W: Write>(out: &mut W) -> Result<()> {
    service::stop(&SystemLaunchctl)?;
    writeln!(out, "sent SIGTERM to com.textlog.agent")?;
    Ok(())
}

fn run_status<W: Write>(out: &mut W) -> Result<()> {
    match service::status(&SystemLaunchctl)? {
        ServiceStatus::NotInstalled => {
            writeln!(out, "status: not installed (run `tl install`)")?;
        }
        ServiceStatus::Installed {
            loaded,
            pid,
            last_exit_code,
        } => {
            writeln!(
                out,
                "status: installed; loaded={loaded}; pid={pid:?}; last_exit={last_exit_code:?}"
            )?;
        }
    }
    Ok(())
}

async fn run_start_foreground(cfg: &Config) -> Result<()> {
    use crate::clipboard;

    let storage = Arc::new(open_storage(cfg)?);
    let notifier = Arc::new(SystemNotifier::new(cfg.notifications.clone()));
    let token = Arc::new(AtomicI64::new(clipboard::current_change_count()));
    let writer = Arc::new(SystemClipboardWriter::new(Arc::clone(&token)));

    let pipeline = Arc::new(Pipeline::new(
        cfg.clone(),
        storage,
        notifier,
        writer,
        token,
    )?);

    eprintln!(
        "textlog: starting foreground pipeline (poll {}ms, log {}). Ctrl-C to stop.",
        cfg.monitoring.poll_interval_ms,
        expand_tilde(&cfg.storage.log_dir).display(),
    );
    pipeline.run().await
}

fn resolve_config_path(override_dir: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(d) = override_dir {
        return Ok(d.join("config.toml"));
    }
    crate::config::default_config_path()
}

fn open_storage(cfg: &Config) -> Result<Storage> {
    let path = expand_tilde(&cfg.storage.sqlite_path);
    Storage::open(path, cfg.storage.ring_buffer_size)
}

// ---- handlers --------------------------------------------------------

async fn run_mcp(cfg: &Config) -> Result<()> {
    let storage = open_storage(cfg)?;
    crate::mcp::run_stdio(Arc::new(storage)).await
}

pub fn print_version<W: Write>(out: &mut W) -> Result<()> {
    writeln!(out, "textlog {}", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

pub fn run_config<W: Write>(
    cmd: ConfigCmd,
    cfg: &Config,
    cfg_path: &Path,
    out: &mut W,
) -> Result<()> {
    match cmd {
        ConfigCmd::Show => {
            let toml = toml::to_string_pretty(cfg)?;
            out.write_all(toml.as_bytes())?;
            Ok(())
        }
        ConfigCmd::Path => {
            writeln!(out, "{}", cfg_path.display())?;
            Ok(())
        }
        ConfigCmd::Reset => {
            let defaults = Config::default();
            crate::config::save_to(cfg_path, &defaults)?;
            writeln!(out, "wrote defaults to {}", cfg_path.display())?;
            Ok(())
        }
    }
}

pub fn run_logs<W: Write>(cmd: LogsCmd, cfg: &Config, out: &mut W) -> Result<()> {
    match cmd {
        LogsCmd::Today => {
            let storage = open_storage(cfg)?;
            let cutoff = today_midnight_utc();
            let rows = storage.get_recent(u32::MAX, None)?;
            let today: Vec<_> = rows.into_iter().filter(|r| r.ts >= cutoff).collect();
            print_rows(out, &today)
        }
        LogsCmd::Search { query, limit } => {
            let storage = open_storage(cfg)?;
            let hits = storage.search(&query, limit, None)?;
            // Print only the canonical row of each duplicate cluster — match
            // the spec's "Claude receives each unique piece only once".
            let unique: Vec<CaptureRow> =
                hits.into_iter().filter(|h| h.duplicate_of.is_none()).map(|h| h.row).collect();
            print_rows(out, &unique)
        }
        LogsCmd::Path => {
            writeln!(out, "{}", expand_tilde(&cfg.storage.log_dir).display())?;
            Ok(())
        }
    }
}

fn print_unimplemented<W: Write>(name: &str, out: &mut W) -> Result<()> {
    writeln!(
        out,
        "`tl {name}` is not yet implemented (Phase 7+ pending — see docs/superpowers/plans)"
    )?;
    Err(Error::ClipboardAccess(format!(
        "tl {name} requires the pipeline / LaunchAgent phases"
    )))
}

fn print_rows<W: Write>(out: &mut W, rows: &[CaptureRow]) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "(no captures)")?;
        return Ok(());
    }
    for r in rows {
        let preview = match (&r.content, r.kind) {
            (Some(t), _) => one_line_preview(t, 80),
            (None, Kind::Image) => "<image — no OCR yet>".into(),
            (None, _) => String::new(),
        };
        writeln!(
            out,
            "{}  {:<5} sha:{}  {}",
            r.ts.to_rfc3339(),
            r.kind.as_str(),
            &hex_lower(&r.sha256)[..8],
            preview,
        )?;
    }
    Ok(())
}

fn one_line_preview(s: &str, max: usize) -> String {
    let single = s.replace(['\n', '\r'], " ");
    if single.chars().count() <= max {
        return single;
    }
    let truncated: String = single.chars().take(max - 1).collect();
    format!("{truncated}…")
}

fn today_midnight_utc() -> chrono::DateTime<chrono::Utc> {
    let now = chrono::Utc::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    fn cfg_with_tmp(tmp: &TempDir) -> Config {
        let mut cfg = Config::default();
        cfg.storage.sqlite_path = tmp
            .path()
            .join("index.db")
            .to_string_lossy()
            .into_owned();
        cfg.storage.log_dir = tmp.path().join("logs").to_string_lossy().into_owned();
        cfg
    }

    fn make_row(ts: chrono::DateTime<Utc>, sha: u8, content: &str, md_dir: &std::path::Path) -> CaptureRow {
        CaptureRow {
            id: 0,
            ts,
            kind: Kind::Text,
            sha256: [sha; 32],
            size_bytes: content.len(),
            content: Some(content.to_string()),
            ocr_confidence: None,
            source_app: None,
            source_url: None,
            md_path: md_dir.join("2026-04-17.md"),
        }
    }

    #[test]
    fn version_prints_cargo_pkg_version() {
        let mut buf = Vec::new();
        print_version(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("textlog "));
        assert!(out.trim().ends_with(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn config_show_emits_valid_toml() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let cfg_path = tmp.path().join("config.toml");
        let mut buf = Vec::new();
        run_config(ConfigCmd::Show, &cfg, &cfg_path, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let parsed: Config = toml::from_str(&out).expect("emitted TOML must parse");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn config_path_prints_resolved_path() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let cfg_path = tmp.path().join("nested").join("config.toml");
        let mut buf = Vec::new();
        run_config(ConfigCmd::Path, &cfg, &cfg_path, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.trim().ends_with("config.toml"));
    }

    #[test]
    fn config_reset_writes_default() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let cfg_path = tmp.path().join("nested").join("config.toml");
        let mut buf = Vec::new();
        run_config(ConfigCmd::Reset, &cfg, &cfg_path, &mut buf).unwrap();
        assert!(cfg_path.exists(), "reset must write the file");
        let written = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed: Config = toml::from_str(&written).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn logs_path_prints_log_dir_expanded() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let mut buf = Vec::new();
        run_logs(LogsCmd::Path, &cfg, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(tmp.path().to_str().unwrap()));
    }

    #[test]
    fn logs_today_returns_no_captures_message_on_empty_db() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let mut buf = Vec::new();
        run_logs(LogsCmd::Today, &cfg, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("(no captures)"));
    }

    #[test]
    fn logs_search_returns_matches() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        // Seed a row.
        let storage = open_storage(&cfg).unwrap();
        storage
            .insert(&make_row(Utc::now(), 1, "needle in haystack", tmp.path()))
            .unwrap();

        let mut buf = Vec::new();
        run_logs(
            LogsCmd::Search {
                query: "needle".into(),
                limit: 10,
            },
            &cfg,
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("needle in haystack"));
    }

    #[test]
    fn logs_today_prints_only_todays_rows() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let storage = open_storage(&cfg).unwrap();
        let yesterday = Utc::now() - chrono::Duration::days(1);
        let now = Utc::now();
        storage.insert(&make_row(yesterday, 1, "old line", tmp.path())).unwrap();
        storage.insert(&make_row(now, 2, "fresh line", tmp.path())).unwrap();

        let mut buf = Vec::new();
        run_logs(LogsCmd::Today, &cfg, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("fresh line"));
        assert!(!out.contains("old line"));
    }

    #[test]
    fn logs_search_dedupes_by_canonical() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let storage = open_storage(&cfg).unwrap();
        // Two rows, same sha256 + same content.
        let now = Utc.with_ymd_and_hms(2026, 4, 17, 10, 0, 0).unwrap();
        storage.insert(&make_row(now, 9, "needle dup", tmp.path())).unwrap();
        storage
            .insert(&make_row(
                now + chrono::Duration::seconds(60),
                9,
                "needle dup",
                tmp.path(),
            ))
            .unwrap();

        let mut buf = Vec::new();
        run_logs(
            LogsCmd::Search {
                query: "needle".into(),
                limit: 10,
            },
            &cfg,
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let occurrences = out.matches("needle dup").count();
        assert_eq!(occurrences, 1, "dup elided; got:\n{out}");
    }

    #[test]
    fn one_line_preview_truncates_with_ellipsis() {
        let s = "x".repeat(200);
        let p = one_line_preview(&s, 80);
        assert_eq!(p.chars().count(), 80);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn one_line_preview_collapses_newlines() {
        let p = one_line_preview("line1\nline2", 80);
        assert!(p.contains("line1 line2"));
    }
}
