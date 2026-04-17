//! Generates `com.textlog.agent.plist` for `launchctl bootstrap`.
//!
//! Spec §LaunchAgent plist (generated): plist runs `tl start
//! --foreground` under launchd; RunAtLoad=true so it boots with the
//! user session, KeepAlive=true so launchd respawns on crash.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::expand_tilde;

/// `Label` field — also the launchctl service id under
/// `gui/$UID/com.textlog.agent`.
pub const PLIST_LABEL: &str = "com.textlog.agent";

/// Default install location: `~/Library/LaunchAgents/com.textlog.agent.plist`.
pub fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        Error::Launchctl("home directory unavailable for LaunchAgent install".into())
    })?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{PLIST_LABEL}.plist")))
}

/// Render the plist XML body. `exe` and `log_dir` should already be
/// absolute (caller resolves `current_exe()` / tilde-expansion).
pub fn generate(exe: &Path, log_dir: &Path) -> String {
    let exe_xml = xml_escape(&exe.to_string_lossy());
    let stdout_xml = xml_escape(&log_dir.join("stdout.log").to_string_lossy());
    let stderr_xml = xml_escape(&log_dir.join("stderr.log").to_string_lossy());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_xml}</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{stdout_xml}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_xml}</string>
</dict>
</plist>
"#
    )
}

/// Convenience: resolve an unexpanded `log_dir` (which may start with
/// `~/`) and pass through to `generate`.
pub fn generate_for_config(exe: &Path, log_dir_cfg: &str) -> String {
    generate(exe, &expand_tilde(log_dir_cfg))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_textlog_agent() {
        assert_eq!(PLIST_LABEL, "com.textlog.agent");
    }

    #[test]
    fn plist_path_lands_in_user_launchagents() {
        // We can't assert the absolute home, but we can verify the tail.
        let p = plist_path().unwrap();
        let s = p.to_string_lossy();
        assert!(s.ends_with("Library/LaunchAgents/com.textlog.agent.plist"), "{s}");
    }

    #[test]
    fn generate_includes_exe_and_log_paths() {
        let body = generate(
            Path::new("/usr/local/bin/tl"),
            Path::new("/Users/me/textlog/logs"),
        );
        assert!(body.contains("<string>/usr/local/bin/tl</string>"));
        assert!(body.contains("/Users/me/textlog/logs/stdout.log"));
        assert!(body.contains("/Users/me/textlog/logs/stderr.log"));
        assert!(body.contains("<string>start</string>"));
        assert!(body.contains("<string>--foreground</string>"));
        assert!(body.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(body.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(body.contains("com.textlog.agent"));
    }

    #[test]
    fn xml_escape_handles_meta_chars() {
        assert_eq!(xml_escape("a&b"), "a&amp;b");
        assert_eq!(xml_escape("<x>"), "&lt;x&gt;");
        assert_eq!(xml_escape("path \"with quotes\""), "path &quot;with quotes&quot;");
        assert_eq!(xml_escape("don't"), "don&apos;t");
    }

    #[test]
    fn generate_escapes_xml_in_paths() {
        let body = generate(Path::new("/tmp/my <weird> path"), Path::new("/tmp/logs"));
        assert!(body.contains("/tmp/my &lt;weird&gt; path"));
        assert!(!body.contains("<weird>"));
    }

    #[test]
    fn generate_is_well_formed_xml_prelude() {
        let body = generate(Path::new("/x/tl"), Path::new("/x/logs"));
        assert!(body.starts_with("<?xml version=\"1.0\""));
        assert!(body.contains("<!DOCTYPE plist"));
        assert!(body.trim_end().ends_with("</plist>"));
    }
}
