//! `tl doctor` — health checks across config / storage / permissions
//! / LaunchAgent / MCP registration / Apple Vision OCR.
//!
//! Each check returns a `Check` with a status (Pass / Warn / Fail)
//! and a one-line detail. `run_all` runs them all in order, prints a
//! table, and returns `Err(Error::Doctor)` if any blocker fails — the
//! main loop maps that to a non-zero exit code.

use std::io::Write;
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::macos_perm::{
    notification_state, pasteboard_access_state, PermissionState,
};
use crate::service::{self, ServiceStatus, SystemLaunchctl};
use crate::storage::{expand_tilde, Storage};

const VISION_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/blank-16x16.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    fn marker(self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

impl Check {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: CheckStatus::Pass, detail: detail.into() }
    }
    pub fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: CheckStatus::Warn, detail: detail.into() }
    }
    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: CheckStatus::Fail, detail: detail.into() }
    }
}

/// Run every doctor check, print the report, and return an error if
/// anything failed. Used by `tl doctor`.
pub fn run_all<W: Write>(cfg: &Config, cfg_path: &Path, out: &mut W) -> Result<()> {
    let checks = collect_checks(cfg, cfg_path);
    print_report(&checks, out)?;
    if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        return Err(Error::Doctor("one or more doctor checks failed".into()));
    }
    Ok(())
}

pub fn collect_checks(cfg: &Config, cfg_path: &Path) -> Vec<Check> {
    vec![
        check_config_file(cfg_path),
        check_log_dir(cfg),
        check_sqlite(cfg),
        check_pasteboard(),
        check_notifications(),
        check_launchagent(),
        check_mcp_registration(),
        check_vision_smoke(cfg),
    ]
}

fn print_report<W: Write>(checks: &[Check], out: &mut W) -> Result<()> {
    let max = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in checks {
        writeln!(
            out,
            "{:<6} {:<width$} — {}",
            c.status.marker(),
            c.name,
            c.detail,
            width = max,
        )?;
    }
    let failed = checks.iter().filter(|c| c.status == CheckStatus::Fail).count();
    let warned = checks.iter().filter(|c| c.status == CheckStatus::Warn).count();
    let passed = checks.iter().filter(|c| c.status == CheckStatus::Pass).count();
    writeln!(out, "\n{passed} pass, {warned} warn, {failed} fail")?;
    Ok(())
}

// ---- individual checks ----------------------------------------------

fn check_config_file(cfg_path: &Path) -> Check {
    let name = "config file";
    if !cfg_path.exists() {
        return Check::fail(name, format!("missing: {} (run `tl config reset`)", cfg_path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(cfg_path) {
            Ok(meta) => {
                let mode = meta.permissions().mode() & 0o777;
                if mode == 0o600 {
                    Check::pass(name, format!("{} (mode 600)", cfg_path.display()))
                } else {
                    Check::warn(
                        name,
                        format!(
                            "{} has permissions {mode:o}; recommend chmod 600 to keep API keys private",
                            cfg_path.display(),
                        ),
                    )
                }
            }
            Err(e) => Check::warn(name, format!("could not stat {}: {e}", cfg_path.display())),
        }
    }
    #[cfg(not(unix))]
    Check::pass(name, format!("{} (perm check skipped — non-Unix)", cfg_path.display()))
}

fn check_log_dir(cfg: &Config) -> Check {
    let name = "log dir writable";
    let dir = expand_tilde(&cfg.storage.log_dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Check::fail(name, format!("{}: {e}", dir.display()));
    }
    let probe = dir.join(".tl-doctor-write-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::pass(name, dir.display().to_string())
        }
        Err(e) => Check::fail(name, format!("{}: {e}", dir.display())),
    }
}

fn check_sqlite(cfg: &Config) -> Check {
    let name = "sqlite + FTS5";
    let path = expand_tilde(&cfg.storage.sqlite_path);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Check::fail(name, format!("could not create {}: {e}", parent.display()));
        }
    }
    match Storage::open(&path, cfg.storage.ring_buffer_size) {
        Ok(s) => {
            // Round-trip a search to confirm FTS5 is actually compiled in.
            match s.search("__tl_doctor_probe__", 1, None) {
                Ok(_) => Check::pass(name, format!("{} (FTS5 ok)", path.display())),
                Err(e) => Check::fail(name, format!("FTS5 query failed: {e}")),
            }
        }
        Err(e) => Check::fail(name, format!("{}: {e}", path.display())),
    }
}

fn check_pasteboard() -> Check {
    let name = "clipboard access";
    match pasteboard_access_state() {
        PermissionState::Granted => Check::pass(name, "NSPasteboard.changeCount succeeded"),
        PermissionState::Denied => Check::fail(
            name,
            "NSPasteboard read failed — grant Pasteboard access in System Settings → Privacy & Security",
        ),
        PermissionState::Unknown => {
            Check::warn(name, "permission state could not be probed (non-macOS or sandbox)")
        }
    }
}

fn check_notifications() -> Check {
    let name = "notifications";
    match notification_state() {
        PermissionState::Granted => Check::pass(name, "UNUserNotificationCenter authorised"),
        PermissionState::Denied => Check::fail(
            name,
            "notifications denied — enable in System Settings → Notifications",
        ),
        PermissionState::Unknown => Check::warn(
            name,
            "permission unknown (UN center requires bundled app); first notification will prompt the user",
        ),
    }
}

fn check_launchagent() -> Check {
    let name = "launchagent";
    match service::status(&SystemLaunchctl) {
        Ok(ServiceStatus::NotInstalled) => {
            Check::warn(name, "not installed (run `tl install` to register the LaunchAgent)")
        }
        Ok(ServiceStatus::Installed { loaded, pid, last_exit_code }) => {
            let detail = format!(
                "loaded={loaded} pid={pid:?} last_exit={last_exit_code:?}"
            );
            if loaded {
                Check::pass(name, detail)
            } else {
                Check::warn(name, format!("plist on disk but not loaded — `tl start` to bootstrap. {detail}"))
            }
        }
        Err(e) => Check::fail(name, format!("status query failed: {e}")),
    }
}

fn check_mcp_registration() -> Check {
    let name = "mcp registration";
    // Detect the `claude` CLI before invoking it.
    let which = std::process::Command::new("which").arg("claude").output();
    let claude_present = matches!(which, Ok(o) if o.status.success() && !o.stdout.is_empty());
    if !claude_present {
        return Check::warn(
            name,
            "`claude` CLI not on PATH — install Claude Code and run `claude mcp add textlog -- tl mcp`",
        );
    }
    let out = std::process::Command::new("claude").arg("mcp").arg("list").output();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.lines().any(|l| l.contains("textlog")) {
                Check::pass(name, "found in `claude mcp list`")
            } else {
                Check::warn(
                    name,
                    "not registered — run `claude mcp add textlog -- tl mcp`",
                )
            }
        }
        Ok(o) => Check::warn(
            name,
            format!(
                "`claude mcp list` returned {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        ),
        Err(e) => Check::warn(name, format!("could not run claude: {e}")),
    }
}

fn check_vision_smoke(cfg: &Config) -> Check {
    let name = "apple vision";
    match crate::ocr::ocr_image(VISION_FIXTURE, &cfg.ocr) {
        Ok(r) => Check::pass(
            name,
            format!(
                "ocr_image succeeded ({} blocks, mean confidence {:.2})",
                r.block_count, r.confidence,
            ),
        ),
        Err(e) => Check::fail(name, format!("OCR smoke failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg_with_tmp(tmp: &TempDir) -> Config {
        let mut cfg = Config::default();
        cfg.storage.sqlite_path = tmp.path().join("index.db").to_string_lossy().into_owned();
        cfg.storage.log_dir = tmp.path().join("logs").to_string_lossy().into_owned();
        cfg
    }

    #[test]
    fn check_config_file_missing_is_fail() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("does-not-exist.toml");
        let c = check_config_file(&p);
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(c.detail.contains("missing"));
    }

    #[cfg(unix)]
    #[test]
    fn check_config_file_warns_on_loose_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        std::fs::write(&p, "x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let c = check_config_file(&p);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.contains("644"));
    }

    #[cfg(unix)]
    #[test]
    fn check_config_file_passes_on_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("config.toml");
        std::fs::write(&p, "x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = check_config_file(&p);
        assert_eq!(c.status, CheckStatus::Pass);
    }

    #[test]
    fn check_log_dir_creates_and_writes_probe() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let c = check_log_dir(&cfg);
        assert_eq!(c.status, CheckStatus::Pass);
        assert!(tmp.path().join("logs").exists());
        // The probe file must be cleaned up on success.
        assert!(!tmp.path().join("logs/.tl-doctor-write-probe").exists());
    }

    #[test]
    fn check_log_dir_fails_when_parent_unwritable() {
        // /private (a system path) is not writable by a normal test
        // run; if for some reason it IS, skip.
        let mut cfg = Config::default();
        cfg.storage.log_dir = "/private/textlog-doctor-bad".into();
        let c = check_log_dir(&cfg);
        assert_eq!(c.status, CheckStatus::Fail, "expected fail; got: {c:?}");
    }

    #[test]
    fn check_sqlite_passes_with_fresh_db() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let c = check_sqlite(&cfg);
        assert_eq!(c.status, CheckStatus::Pass);
        assert!(c.detail.contains("FTS5"));
    }

    #[test]
    fn run_all_returns_err_when_any_check_fails() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = cfg_with_tmp(&tmp);
        cfg.storage.log_dir = "/private/cannot-write".into();
        let bad_cfg_path = tmp.path().join("nope.toml");
        let mut buf = Vec::new();
        let r = run_all(&cfg, &bad_cfg_path, &mut buf);
        assert!(r.is_err(), "expected Err on failed checks");
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("FAIL"));
    }

    #[test]
    fn print_report_lists_each_check_once() {
        let mut buf = Vec::new();
        let checks = vec![
            Check::pass("a", "ok"),
            Check::warn("b", "meh"),
            Check::fail("c", "broken"),
        ];
        print_report(&checks, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("PASS"));
        assert!(s.contains("WARN"));
        assert!(s.contains("FAIL"));
        assert!(s.contains("1 pass, 1 warn, 1 fail"));
    }
}
