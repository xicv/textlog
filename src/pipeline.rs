//! Capture pipeline: clipboard event → privacy filter → OCR (images
//! only) → SHA-256 → Storage::insert → notifier + clipboard write-back.
//!
//! `Pipeline::process_event` is the unit-testable core; `Pipeline::run`
//! adds the polling task and the consumer loop and is exercised by
//! `tl start --foreground`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::clipboard::{self, ClipboardEvent, ClipboardWriter};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::filters::PrivacyFilter;
use crate::notifier::Notifier;
use crate::storage::{markdown, CaptureRow, Kind, Storage};

const CHANNEL_CAPACITY: usize = 16;

pub struct Pipeline {
    cfg: Config,
    storage: Arc<Storage>,
    notifier: Arc<dyn Notifier>,
    clipboard_writer: Arc<dyn ClipboardWriter>,
    self_write_token: Arc<AtomicI64>,
    privacy_filter: PrivacyFilter,
}

impl Pipeline {
    pub fn new(
        cfg: Config,
        storage: Arc<Storage>,
        notifier: Arc<dyn Notifier>,
        clipboard_writer: Arc<dyn ClipboardWriter>,
        self_write_token: Arc<AtomicI64>,
    ) -> Result<Self> {
        let privacy_filter = PrivacyFilter::from_config(&cfg.monitoring, &cfg.privacy)?;
        Ok(Self {
            cfg,
            storage,
            notifier,
            clipboard_writer,
            self_write_token,
            privacy_filter,
        })
    }

    /// Process one event: filter → OCR → store → notify. Returns `Ok`
    /// even when the event is intentionally dropped (empty bytes,
    /// sub-min length, sensitive content). Hard errors (storage write
    /// failed, OCR failed) bubble up.
    pub async fn process_event(&self, ev: ClipboardEvent) -> Result<()> {
        if ev.bytes.is_empty() {
            return Ok(());
        }

        // Min-length only applies to text (an image of any size is
        // signal even if the file is small).
        if ev.kind == Kind::Text && ev.bytes.len() < self.cfg.monitoring.min_length {
            return Ok(());
        }

        // Privacy filter — text only.
        if ev.kind == Kind::Text {
            let s = std::str::from_utf8(&ev.bytes).unwrap_or("");
            if self.privacy_filter.is_sensitive(s) {
                if self.cfg.privacy.show_filter_notification {
                    let _ = self
                        .notifier
                        .notify_capture("textlog dropped a sensitive clipboard entry");
                }
                return Ok(());
            }
        }

        // OCR (images only). Vision is sync — push it off the executor.
        let (content, ocr_confidence) = match ev.kind {
            Kind::Text => (
                Some(String::from_utf8_lossy(&ev.bytes).into_owned()),
                None,
            ),
            Kind::Image => {
                let cfg = self.cfg.ocr.clone();
                let bytes = ev.bytes.clone();
                let r = tokio::task::spawn_blocking(move || crate::ocr::ocr_image(&bytes, &cfg))
                    .await
                    .map_err(|e| Error::Ocr(format!("ocr task join: {e}")))??;
                (Some(r.text), Some(r.confidence))
            }
            Kind::File => (None, None),
        };

        // Hash + build CaptureRow.
        let mut h = Sha256::new();
        h.update(&ev.bytes);
        let sha: [u8; 32] = h.finalize().into();
        let ts = Utc::now();
        let md_path = markdown::daily_path(
            &self.cfg.storage.log_dir,
            &self.cfg.storage.date_format,
            ts,
        );
        let row = CaptureRow {
            id: 0,
            ts,
            kind: ev.kind,
            sha256: sha,
            size_bytes: ev.bytes.len(),
            content,
            ocr_confidence,
            source_app: None,
            source_url: None,
            md_path: md_path.clone(),
        };

        // Storage::insert is sync (mutex on rusqlite::Connection); push
        // it off the executor so concurrent MCP requests aren't
        // blocked.
        let storage = Arc::clone(&self.storage);
        let row_clone = row.clone();
        tokio::task::spawn_blocking(move || storage.insert(&row_clone))
            .await
            .map_err(|e| Error::Storage(format!("insert task join: {e}")))??;

        // Per-capture notification (default off).
        if self.cfg.notifications.enabled && self.cfg.notifications.on_capture {
            let summary = match ev.kind {
                Kind::Text => "captured text",
                Kind::Image => "captured image",
                Kind::File => "captured file",
            };
            let _ = self.notifier.notify_capture(summary);
        }

        // Completion notification + optional path-back-to-clipboard.
        let _ = self.notifier.notify_complete(&md_path);
        if self.cfg.notifications.copy_log_path_on_complete {
            let path_str = md_path.to_string_lossy().into_owned();
            let writer = Arc::clone(&self.clipboard_writer);
            let _ = tokio::task::spawn_blocking(move || writer.write_text(&path_str))
                .await
                .map_err(|e| Error::ClipboardAccess(format!("write_text join: {e}")))?;
        }

        Ok(())
    }

    /// Run forever: poll the system clipboard, push events through a
    /// bounded channel, drain into `process_event`. Returns when either
    /// task exits (typically on shutdown signal).
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<ClipboardEvent>(CHANNEL_CAPACITY);
        let interval = Duration::from_millis(self.cfg.monitoring.poll_interval_ms.max(50));
        let token = Arc::clone(&self.self_write_token);

        let monitor = tokio::spawn(monitor_loop(interval, token, tx));
        let consumer = {
            let me = Arc::clone(&self);
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if let Err(e) = me.process_event(ev).await {
                        tracing::error!(?e, "pipeline process_event failed");
                    }
                }
            })
        };

        tokio::select! {
            r = monitor => {
                r.map_err(|e| Error::ClipboardAccess(format!("monitor task panicked: {e}")))?;
            }
            r = consumer => {
                r.map_err(|e| Error::Storage(format!("consumer task panicked: {e}")))?;
            }
        }
        Ok(())
    }
}

async fn monitor_loop(
    base_interval: Duration,
    self_write_token: Arc<AtomicI64>,
    tx: mpsc::Sender<ClipboardEvent>,
) {
    // Two-tier polling. macOS has no NSPasteboard event API (see
    // `clipboard.rs` preamble), so we poll. But humans don't copy four
    // times per second, so running at the active rate 24/7 burns
    // wakeups for nothing. Active rate = `base_interval`; after
    // `BACKOFF_AFTER` consecutive unchanged ticks we double the sleep
    // up to `max_interval`, and any real change resets back to active.
    const BACKOFF_AFTER: u32 = 20;
    let max_interval = base_interval
        .saturating_mul(4)
        .min(Duration::from_secs(2));

    let mut last = clipboard::current_change_count();
    let mut current = base_interval;
    let mut idle_ticks: u32 = 0;

    loop {
        tokio::time::sleep(current).await;

        // Fast path: `changeCount` is an i64 ObjC property read
        // (microseconds). Wrapping it in spawn_blocking on every tick
        // pays a task alloc + context switch for no reason — do it
        // directly on the async task instead.
        let cc = clipboard::current_change_count();
        if cc <= last {
            idle_ticks = idle_ticks.saturating_add(1);
            if idle_ticks >= BACKOFF_AFTER && current < max_interval {
                current = current.saturating_mul(2).min(max_interval);
            }
            continue;
        }

        // Change detected — resume active rate.
        idle_ticks = 0;
        current = base_interval;

        // Self-write? Mirror `last` forward but don't enqueue.
        if cc == self_write_token.load(Ordering::SeqCst) {
            last = cc;
            continue;
        }

        // Slow path: the actual content read (NSString / PNG bytes)
        // does enough FFI work to warrant the blocking pool.
        let token = Arc::clone(&self_write_token);
        let result =
            tokio::task::spawn_blocking(move || clipboard::poll_once(&token, last)).await;
        match result {
            Ok(Ok(Some(ev))) => {
                last = ev.change_count;
                if ev.bytes.is_empty() {
                    continue;
                }
                if tx.try_send(ev).is_err() {
                    tracing::warn!(
                        "clipboard channel full ({CHANNEL_CAPACITY}); dropping event"
                    );
                }
            }
            Ok(Ok(None)) => {
                // Race: a newer change (often our own write) slipped
                // in between our fast-path read and `poll_once`'s
                // check. Keep `last` in sync with what we observed so
                // we don't re-enter the slow path every tick.
                last = cc;
            }
            Ok(Err(e)) => tracing::error!(?e, "clipboard poll error"),
            Err(e) => {
                tracing::error!(?e, "clipboard poll task panicked");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{ClipboardEvent, CountingClipboardWriter, NullClipboardWriter};
    use crate::notifier::CountingNotifier;
    use std::path::PathBuf;
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

    fn build_pipeline(
        cfg: Config,
        tmp: &TempDir,
        notifier: Arc<CountingNotifier>,
        writer: Arc<CountingClipboardWriter>,
    ) -> (Arc<Pipeline>, PathBuf) {
        let storage = Arc::new(
            Storage::open(tmp.path().join("index.db"), cfg.storage.ring_buffer_size).unwrap(),
        );
        let token = Arc::new(AtomicI64::new(0));
        let p = Pipeline::new(
            cfg,
            Arc::clone(&storage) as Arc<Storage>,
            notifier as Arc<dyn Notifier>,
            writer as Arc<dyn ClipboardWriter>,
            token,
        )
        .unwrap();
        (Arc::new(p), tmp.path().to_path_buf())
    }

    fn text_event(s: &str, cc: i64) -> ClipboardEvent {
        ClipboardEvent {
            kind: Kind::Text,
            bytes: s.as_bytes().to_vec(),
            change_count: cc,
        }
    }

    #[tokio::test]
    async fn empty_event_is_dropped_silently() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_tmp(&tmp);
        let notifier = Arc::new(CountingNotifier::new());
        let writer = Arc::new(CountingClipboardWriter::new());
        let (p, _) = build_pipeline(cfg, &tmp, Arc::clone(&notifier), Arc::clone(&writer));

        p.process_event(ClipboardEvent {
            kind: Kind::Text,
            bytes: Vec::new(),
            change_count: 1,
        })
        .await
        .unwrap();

        assert_eq!(notifier.completed(), 0);
        assert_eq!(notifier.captured(), 0);
        assert!(writer.calls().is_empty());
    }

    #[tokio::test]
    async fn sub_min_length_text_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = cfg_with_tmp(&tmp);
        cfg.monitoring.min_length = 50;
        let notifier = Arc::new(CountingNotifier::new());
        let writer = Arc::new(NullClipboardWriter);
        let (p, _) = build_pipeline(
            cfg,
            &tmp,
            Arc::clone(&notifier),
            Arc::new(CountingClipboardWriter::new()),
        );

        let _ = writer; // silence unused
        p.process_event(text_event("short", 1)).await.unwrap();
        assert_eq!(notifier.completed(), 0);
    }

    #[tokio::test]
    async fn normal_text_event_inserts_and_notifies() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = cfg_with_tmp(&tmp);
        cfg.monitoring.min_length = 1;
        cfg.privacy.filter_enabled = false;
        cfg.notifications.copy_log_path_on_complete = false;
        let notifier = Arc::new(CountingNotifier::new());
        let writer = Arc::new(CountingClipboardWriter::new());
        let (p, _) = build_pipeline(cfg, &tmp, Arc::clone(&notifier), Arc::clone(&writer));

        p.process_event(text_event("hello world", 1)).await.unwrap();

        assert_eq!(notifier.completed(), 1);
        assert!(writer.calls().is_empty(), "no copy-back when flag is off");

        // Re-open same storage to confirm the row landed.
        let storage = Storage::open(tmp.path().join("index.db"), 100).unwrap();
        let rows = storage.get_recent(10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content.as_deref(), Some("hello world"));
    }

    #[tokio::test]
    async fn sensitive_text_is_filtered_and_notified() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = cfg_with_tmp(&tmp);
        cfg.monitoring.min_length = 1;
        cfg.privacy.filter_enabled = true;
        cfg.privacy.show_filter_notification = true;
        let notifier = Arc::new(CountingNotifier::new());
        let writer = Arc::new(NullClipboardWriter);
        let (p, _) = build_pipeline(
            cfg,
            &tmp,
            Arc::clone(&notifier),
            Arc::new(CountingClipboardWriter::new()),
        );

        let _ = writer;
        // OpenAI-style key — matches default ignore patterns.
        p.process_event(text_event("sk-1234567890abcdefghij", 1))
            .await
            .unwrap();

        assert_eq!(notifier.captured(), 1, "filter notification fired");
        assert_eq!(notifier.completed(), 0, "no insert → no completion");

        let storage = Storage::open(tmp.path().join("index.db"), 100).unwrap();
        assert!(storage.get_recent(10, None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn copy_log_path_writes_clipboard_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = cfg_with_tmp(&tmp);
        cfg.monitoring.min_length = 1;
        cfg.privacy.filter_enabled = false;
        cfg.notifications.enabled = true;
        cfg.notifications.on_complete = true;
        cfg.notifications.copy_log_path_on_complete = true;
        let notifier = Arc::new(CountingNotifier::new());
        let writer = Arc::new(CountingClipboardWriter::new());
        let (p, _) = build_pipeline(cfg, &tmp, Arc::clone(&notifier), Arc::clone(&writer));

        p.process_event(text_event("a useful clipboard payload", 1))
            .await
            .unwrap();

        assert_eq!(notifier.completed(), 1);
        let calls = writer.calls();
        assert_eq!(calls.len(), 1, "expected one clipboard write-back");
        assert!(
            calls[0].ends_with(".md"),
            "wrote the daily MD path; got {:?}",
            calls[0]
        );
    }

    #[tokio::test]
    async fn on_capture_notification_fires_when_configured() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = cfg_with_tmp(&tmp);
        cfg.monitoring.min_length = 1;
        cfg.privacy.filter_enabled = false;
        cfg.notifications.enabled = true;
        cfg.notifications.on_capture = true;
        cfg.notifications.copy_log_path_on_complete = false;
        let notifier = Arc::new(CountingNotifier::new());
        let (p, _) = build_pipeline(
            cfg,
            &tmp,
            Arc::clone(&notifier),
            Arc::new(CountingClipboardWriter::new()),
        );

        p.process_event(text_event("payload", 1)).await.unwrap();

        assert_eq!(notifier.captured(), 1, "on_capture should fire");
        assert_eq!(notifier.completed(), 1);
    }
}
