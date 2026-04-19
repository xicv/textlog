//! Thin wrapper around the `launchctl` CLI.
//!
//! Behind a trait so `service::*` callers can swap in a recording
//! double in tests instead of mutating the real launchd state.

use std::process::Command;
#[cfg(test)]
use std::sync::Mutex;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct LaunchctlOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait LaunchctlRunner: Send + Sync {
    fn run(&self, args: &[&str]) -> Result<LaunchctlOutput>;
}

/// Real runner — shells out to `/bin/launchctl`.
pub struct SystemLaunchctl;

impl LaunchctlRunner for SystemLaunchctl {
    fn run(&self, args: &[&str]) -> Result<LaunchctlOutput> {
        let out = Command::new("launchctl").args(args).output().map_err(|e| {
            Error::Launchctl(format!("failed to spawn launchctl: {e}"))
        })?;
        Ok(LaunchctlOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Test double: records every invocation and returns either a per-arg
/// canned `LaunchctlOutput` (set via `set_response`) or the default
/// `success` output.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingLaunchctl {
    pub calls: Mutex<Vec<Vec<String>>>,
    /// First-arg-keyed canned responses (e.g. "print" → simulated
    /// `launchctl print` stdout).
    pub responses: Mutex<std::collections::HashMap<String, LaunchctlOutput>>,
}

#[cfg(test)]
impl RecordingLaunchctl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed the response for a given first-arg verb (e.g. `"print"`).
    pub fn set_response(&self, first_arg: &str, out: LaunchctlOutput) {
        self.responses.lock().unwrap().insert(first_arg.to_string(), out);
    }

    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl LaunchctlRunner for RecordingLaunchctl {
    fn run(&self, args: &[&str]) -> Result<LaunchctlOutput> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.calls.lock().unwrap().push(owned.clone());
        if let Some(first) = args.first() {
            if let Some(out) = self.responses.lock().unwrap().get(*first) {
                return Ok(out.clone());
            }
        }
        Ok(LaunchctlOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// `gui/$UID` domain target launchctl uses for per-user agents.
pub fn user_domain_target() -> String {
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}")
}

/// `gui/$UID/com.textlog.agent` — the fully-qualified service id used
/// by `launchctl bootout`, `kickstart`, `kill`, `print`.
pub fn user_service_target(label: &str) -> String {
    format!("{}/{}", user_domain_target(), label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_service_target_concats_uid_and_label() {
        let s = user_service_target("com.x.y");
        assert!(s.starts_with("gui/"));
        assert!(s.ends_with("/com.x.y"));
    }

    #[test]
    fn recording_runner_captures_args() {
        let r = RecordingLaunchctl::new();
        r.run(&["bootstrap", "gui/501", "/path/to/plist"]).unwrap();
        r.run(&["bootout", "gui/501/com.textlog.agent"]).unwrap();
        let calls = r.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec!["bootstrap", "gui/501", "/path/to/plist"]);
        assert_eq!(calls[1], vec!["bootout", "gui/501/com.textlog.agent"]);
    }

    #[test]
    fn recording_runner_returns_canned_response_for_print() {
        let r = RecordingLaunchctl::new();
        r.set_response(
            "print",
            LaunchctlOutput {
                success: true,
                stdout: "pid = 12345\nlast exit code = 0\n".into(),
                stderr: String::new(),
            },
        );
        let out = r.run(&["print", "gui/501/com.textlog.agent"]).unwrap();
        assert!(out.stdout.contains("pid = 12345"));

        // Other verbs still get the empty default.
        let out2 = r.run(&["kickstart", "gui/501/com.textlog.agent"]).unwrap();
        assert!(out2.success);
        assert!(out2.stdout.is_empty());
    }
}
