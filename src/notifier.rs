//! Capture-pipeline notifications.
//!
//! `notify_capture` fires per spec when `notifications.on_capture = true`
//! (off by default — most users want one summary per session, not a
//! per-event ping). `notify_complete` fires after the storage write so
//! the user can find the daily MD file. The "copy log path to
//! clipboard" side-effect lives in the **clipboard** module so we can
//! share the `Arc<AtomicI64>` self-write token in one place — Notifier
//! just exposes the log path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::schema::NotificationsConfig;
use crate::error::{Error, Result};

/// What the pipeline can ask the notifier to do. Behind a trait so the
/// pipeline takes `Arc<dyn Notifier>` and tests can swap in a counting
/// double instead of firing real OS notifications.
pub trait Notifier: Send + Sync {
    fn notify_capture(&self, summary: &str) -> Result<()>;
    fn notify_complete(&self, log_path: &Path) -> Result<()>;
}

/// Real notifier using `notify-rust`.
///
/// `notify-rust` on macOS targets the (deprecated) `NSUserNotification`
/// API; for an unsigned binary that's the simplest path. When the
/// daemon is bundled (Phase 13+ with a `.app`) we'll switch to
/// `UNUserNotificationCenter`.
pub struct SystemNotifier {
    cfg: NotificationsConfig,
}

impl SystemNotifier {
    pub fn new(cfg: NotificationsConfig) -> Self {
        Self { cfg }
    }
}

impl Notifier for SystemNotifier {
    fn notify_capture(&self, summary: &str) -> Result<()> {
        if !self.cfg.enabled || !self.cfg.on_capture {
            return Ok(());
        }
        send("textlog: captured", summary, self.cfg.sound)
    }

    fn notify_complete(&self, log_path: &Path) -> Result<()> {
        if !self.cfg.enabled || !self.cfg.on_complete {
            return Ok(());
        }
        let body = format!("Saved to {}", log_path.display());
        send("textlog: written", &body, self.cfg.sound)
    }
}

/// Notifier that only counts calls — for unit tests. Doesn't touch
/// the OS notification centre.
#[derive(Debug, Default)]
pub struct CountingNotifier {
    pub captured: AtomicUsize,
    pub completed: AtomicUsize,
    pub last_capture_summary: std::sync::Mutex<Option<String>>,
    pub last_complete_path: std::sync::Mutex<Option<PathBuf>>,
}

impl CountingNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_arc(self) -> Arc<dyn Notifier> {
        Arc::new(self)
    }

    pub fn captured(&self) -> usize {
        self.captured.load(Ordering::SeqCst)
    }

    pub fn completed(&self) -> usize {
        self.completed.load(Ordering::SeqCst)
    }
}

impl Notifier for CountingNotifier {
    fn notify_capture(&self, summary: &str) -> Result<()> {
        self.captured.fetch_add(1, Ordering::SeqCst);
        *self.last_capture_summary.lock().unwrap() = Some(summary.to_string());
        Ok(())
    }

    fn notify_complete(&self, log_path: &Path) -> Result<()> {
        self.completed.fetch_add(1, Ordering::SeqCst);
        *self.last_complete_path.lock().unwrap() = Some(log_path.to_path_buf());
        Ok(())
    }
}

fn send(summary: &str, body: &str, sound: bool) -> Result<()> {
    let mut n = notify_rust::Notification::new();
    n.summary(summary).body(body).appname("textlog");
    if sound {
        // notify-rust does not expose a typed enum for system sounds on
        // macOS; the empty string asks the system for its default tone.
        n.sound_name("default");
    }
    n.show()
        .map(|_handle| ())
        .map_err(|e| Error::Notification(format!("notify-rust dispatch failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, on_capture: bool, on_complete: bool) -> NotificationsConfig {
        NotificationsConfig {
            enabled,
            on_capture,
            on_complete,
            copy_log_path_on_complete: false,
            sound: false,
        }
    }

    #[test]
    fn system_notifier_skips_when_disabled() {
        // enabled=false → both paths return Ok without trying to dispatch.
        let n = SystemNotifier::new(cfg(false, true, true));
        n.notify_capture("ignored").unwrap();
        n.notify_complete(Path::new("/tmp/x.md")).unwrap();
    }

    #[test]
    fn system_notifier_skips_capture_when_on_capture_false() {
        let n = SystemNotifier::new(cfg(true, false, false));
        n.notify_capture("nothing").unwrap();
    }

    #[test]
    fn system_notifier_skips_complete_when_on_complete_false() {
        let n = SystemNotifier::new(cfg(true, false, false));
        n.notify_complete(Path::new("/tmp/x.md")).unwrap();
    }

    #[test]
    fn counting_notifier_records_calls() {
        let c = CountingNotifier::new();
        c.notify_capture("first").unwrap();
        c.notify_capture("second").unwrap();
        c.notify_complete(Path::new("/tmp/log.md")).unwrap();

        assert_eq!(c.captured(), 2);
        assert_eq!(c.completed(), 1);
        assert_eq!(
            c.last_capture_summary.lock().unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(
            c.last_complete_path.lock().unwrap().as_deref(),
            Some(Path::new("/tmp/log.md"))
        );
    }

    #[test]
    fn counting_notifier_can_be_used_via_trait() {
        let c: Arc<dyn Notifier> = CountingNotifier::new().into_arc();
        c.notify_capture("trait dispatch").unwrap();
        c.notify_complete(Path::new("/tmp/x.md")).unwrap();
    }

    /// Smoke-test that hits the real OS notification centre. Gated
    /// because most CI environments cannot pop a notification.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "fires a real macOS notification; run with --ignored"]
    fn system_notifier_dispatches_real_notification() {
        let n = SystemNotifier::new(cfg(true, true, true));
        n.notify_capture("textlog test capture").unwrap();
        n.notify_complete(Path::new("/tmp/test.md")).unwrap();
    }
}
