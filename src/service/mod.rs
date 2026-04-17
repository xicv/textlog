//! LaunchAgent lifecycle: install/uninstall/start/stop/status against
//! a per-user `gui/$UID/com.textlog.agent` service.
//!
//! All operations route through a `LaunchctlRunner` so tests can swap
//! in `RecordingLaunchctl` and assert the exact argv each call
//! produces, without mutating launchd state.

pub mod launchctl;
pub mod plist;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub use launchctl::{LaunchctlRunner, SystemLaunchctl};

/// Reported state of the LaunchAgent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatus {
    NotInstalled,
    Installed {
        loaded: bool,
        pid: Option<u32>,
        last_exit_code: Option<i32>,
    },
}

/// Write the plist + run `launchctl bootstrap`.
///
/// `exe` is the absolute path to the `tl` binary that launchd will
/// invoke (typically `std::env::current_exe()?`). `log_dir` is where
/// stdout/stderr go.
pub fn install<R: LaunchctlRunner>(
    runner: &R,
    exe: &Path,
    log_dir: &Path,
) -> Result<PathBuf> {
    let plist_path = plist::plist_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = plist::generate(exe, log_dir);
    std::fs::write(&plist_path, body)?;

    let domain = launchctl::user_domain_target();
    let path_str = plist_path.to_string_lossy().into_owned();
    let out = runner.run(&["bootstrap", &domain, &path_str])?;
    if !out.success {
        // EEXIST (already loaded) is a soft failure — bootout-then-retry
        // belongs to the caller; we surface the raw stderr.
        return Err(Error::Launchctl(format!(
            "launchctl bootstrap failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(plist_path)
}

/// `launchctl bootout` then remove the plist file.
pub fn uninstall<R: LaunchctlRunner>(runner: &R) -> Result<()> {
    let plist_path = plist::plist_path()?;
    let target = launchctl::user_service_target(plist::PLIST_LABEL);
    let out = runner.run(&["bootout", &target])?;
    // "Could not find service" is the OK-already-gone path; surface
    // anything else.
    if !out.success && !is_not_found_error(&out.stderr) {
        return Err(Error::Launchctl(format!(
            "launchctl bootout failed: {}",
            out.stderr.trim()
        )));
    }
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
    }
    Ok(())
}

/// `launchctl kickstart` — re-runs the program if it had exited.
pub fn start<R: LaunchctlRunner>(runner: &R) -> Result<()> {
    let target = launchctl::user_service_target(plist::PLIST_LABEL);
    let out = runner.run(&["kickstart", &target])?;
    if !out.success {
        return Err(Error::Launchctl(format!(
            "launchctl kickstart failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(())
}

/// `launchctl kill SIGTERM` — KeepAlive will respawn unless `bootout`-ed
/// first.
pub fn stop<R: LaunchctlRunner>(runner: &R) -> Result<()> {
    let target = launchctl::user_service_target(plist::PLIST_LABEL);
    let out = runner.run(&["kill", "SIGTERM", &target])?;
    if !out.success {
        return Err(Error::Launchctl(format!(
            "launchctl kill failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(())
}

/// Combine plist presence + parsed `launchctl print` output.
pub fn status<R: LaunchctlRunner>(runner: &R) -> Result<ServiceStatus> {
    let plist_exists = plist::plist_path()?.exists();
    let target = launchctl::user_service_target(plist::PLIST_LABEL);
    let out = runner.run(&["print", &target])?;

    if !out.success {
        return Ok(if plist_exists {
            // Plist on disk but launchctl doesn't know it — installed
            // but not loaded.
            ServiceStatus::Installed {
                loaded: false,
                pid: None,
                last_exit_code: None,
            }
        } else {
            ServiceStatus::NotInstalled
        });
    }

    Ok(ServiceStatus::Installed {
        loaded: true,
        pid: parse_pid(&out.stdout),
        last_exit_code: parse_last_exit_code(&out.stdout),
    })
}

fn is_not_found_error(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("could not find") || s.contains("no such process")
}

/// Parse a numeric `pid = N` line from `launchctl print` output.
/// Tolerates surrounding whitespace and other lines.
fn parse_pid(stdout: &str) -> Option<u32> {
    extract_numeric(stdout, "pid")
}

fn parse_last_exit_code(stdout: &str) -> Option<i32> {
    for line in stdout.lines() {
        let l = line.trim().to_lowercase();
        if let Some(rest) = l.strip_prefix("last exit code = ") {
            if let Ok(n) = rest.trim().parse::<i32>() {
                return Some(n);
            }
        }
    }
    None
}

fn extract_numeric<T: std::str::FromStr>(stdout: &str, key: &str) -> Option<T> {
    let needle = format!("{key} = ");
    for line in stdout.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix(&needle) {
            if let Ok(n) = rest.trim().parse::<T>() {
                return Some(n);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use launchctl::{LaunchctlOutput, RecordingLaunchctl};

    #[test]
    fn install_runs_bootstrap_against_user_domain() {
        let r = RecordingLaunchctl::new();
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("tl");
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();

        // Will fail because the real plist path is in user's home,
        // but install() writes the plist regardless and then calls
        // bootstrap. We expect at minimum the bootstrap call to land.
        let _ = install(&r, &exe, tmp.path());
        let calls = r.calls();
        assert_eq!(calls.len(), 1, "exactly one launchctl call");
        assert_eq!(calls[0][0], "bootstrap");
        assert!(calls[0][1].starts_with("gui/"));
        assert!(calls[0][2].ends_with("com.textlog.agent.plist"));
    }

    #[test]
    fn install_surfaces_bootstrap_failure() {
        let r = RecordingLaunchctl::new();
        r.set_response(
            "bootstrap",
            LaunchctlOutput {
                success: false,
                stdout: String::new(),
                stderr: "Bootstrap failed: 17 (file exists)".into(),
            },
        );
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("tl");
        std::fs::write(&exe, "x").unwrap();
        let err = install(&r, &exe, tmp.path()).unwrap_err();
        assert!(format!("{err}").contains("Bootstrap failed"));
    }

    #[test]
    fn uninstall_runs_bootout_against_service_target() {
        let r = RecordingLaunchctl::new();
        let _ = uninstall(&r);
        let calls = r.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "bootout");
        assert!(calls[0][1].ends_with("/com.textlog.agent"));
    }

    #[test]
    fn uninstall_treats_not_found_as_success() {
        let r = RecordingLaunchctl::new();
        r.set_response(
            "bootout",
            LaunchctlOutput {
                success: false,
                stdout: String::new(),
                stderr: "Could not find service".into(),
            },
        );
        // Should not return an error.
        uninstall(&r).unwrap();
    }

    #[test]
    fn start_runs_kickstart() {
        let r = RecordingLaunchctl::new();
        start(&r).unwrap();
        let calls = r.calls();
        assert_eq!(calls[0][0], "kickstart");
    }

    #[test]
    fn stop_runs_kill_sigterm() {
        let r = RecordingLaunchctl::new();
        stop(&r).unwrap();
        let calls = r.calls();
        assert_eq!(calls[0], vec!["kill", "SIGTERM", &launchctl::user_service_target(plist::PLIST_LABEL)]);
    }

    #[test]
    fn status_loaded_parses_pid_and_last_exit() {
        let r = RecordingLaunchctl::new();
        r.set_response(
            "print",
            LaunchctlOutput {
                success: true,
                stdout: "service = com.textlog.agent\n  pid = 4242\n  last exit code = 0\n".into(),
                stderr: String::new(),
            },
        );
        match status(&r).unwrap() {
            ServiceStatus::Installed {
                loaded,
                pid,
                last_exit_code,
            } => {
                assert!(loaded);
                assert_eq!(pid, Some(4242));
                assert_eq!(last_exit_code, Some(0));
            }
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn status_print_failure_returns_not_installed_when_no_plist() {
        let r = RecordingLaunchctl::new();
        r.set_response(
            "print",
            LaunchctlOutput {
                success: false,
                stdout: String::new(),
                stderr: "Could not find service".into(),
            },
        );
        // Real plist path likely doesn't exist on the test box.
        // (If it does, the test box already has textlog installed —
        // skip in that case.)
        if plist::plist_path().unwrap().exists() {
            eprintln!("skip: textlog plist already installed on this box");
            return;
        }
        assert_eq!(status(&r).unwrap(), ServiceStatus::NotInstalled);
    }

    #[test]
    fn parse_pid_handles_indented_lines() {
        assert_eq!(parse_pid("foo\n   pid = 99\nbar"), Some(99));
        assert_eq!(parse_pid("no pid here"), None);
    }

    #[test]
    fn parse_last_exit_code_handles_negative() {
        assert_eq!(
            parse_last_exit_code("\tlast exit code = -1\n"),
            Some(-1)
        );
    }

    #[test]
    fn is_not_found_error_matches_common_messages() {
        assert!(is_not_found_error("Could not find service in domain"));
        assert!(is_not_found_error("could not find anything"));
        assert!(!is_not_found_error("Bootstrap failed: 17 (file exists)"));
    }
}
