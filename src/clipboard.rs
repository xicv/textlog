//! NSPasteboard read/write primitives.
//!
//! `poll_once` is the polling primitive used by the pipeline: returns
//! the latest `ClipboardEvent` when `NSPasteboard.changeCount` advances
//! past `last`, and `None` otherwise. Two short-circuits keep us from
//! processing our own writes:
//!
//! 1. `changeCount <= last` — nothing changed since the last poll.
//! 2. `changeCount == self_write_token` — the latest change *is* our
//!    own write (notifier copying the daily-MD path back to the
//!    clipboard, see `notifications.copy_log_path_on_complete`).
//!
//! `write_text` performs the corresponding write and publishes the
//! resulting `changeCount` into the shared `AtomicI64` so the next
//! `poll_once` skips it.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::error::{Error, Result};
use crate::storage::Kind;

/// One observed clipboard transition.
#[derive(Debug, Clone)]
pub struct ClipboardEvent {
    pub kind: Kind,
    /// Raw bytes — UTF-8 for Text, image bytes (PNG / TIFF) for Image.
    pub bytes: Vec<u8>,
    /// `NSPasteboard.changeCount` value at read time. Caller must
    /// pass this back to the next `poll_once` as `last`.
    pub change_count: i64,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
    };
    use objc2_foundation::{NSArray, NSString};

    /// Read the current `changeCount` without inspecting any content.
    /// Cheap enough to call on every poll tick.
    pub fn current_change_count() -> i64 {
        autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount() as i64)
    }

    pub fn poll_once(
        self_write_token: &AtomicI64,
        last: i64,
    ) -> Result<Option<ClipboardEvent>> {
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            let cc = pb.changeCount() as i64;
            if cc <= last {
                return Ok(None);
            }
            if cc == self_write_token.load(Ordering::SeqCst) {
                return Ok(None);
            }

            // Prefer text — even when an image is also present, the
            // user's intent is usually the text (e.g. screenshots that
            // include alt text). Fall through to image if no text.
            if let Some(s) = unsafe { pb.stringForType(NSPasteboardTypeString) } {
                let text = s.to_string();
                if !text.is_empty() {
                    return Ok(Some(ClipboardEvent {
                        kind: Kind::Text,
                        bytes: text.into_bytes(),
                        change_count: cc,
                    }));
                }
            }

            if let Some(data) = unsafe { pb.dataForType(NSPasteboardTypePNG) } {
                let bytes = data.to_vec();
                if !bytes.is_empty() {
                    return Ok(Some(ClipboardEvent {
                        kind: Kind::Image,
                        bytes,
                        change_count: cc,
                    }));
                }
            }

            // Pasteboard advanced but carried no content we handle —
            // return None so the caller still updates its `last` to the
            // new `cc` and we skip re-polling the same change forever.
            Ok(Some(ClipboardEvent {
                kind: Kind::Text,
                bytes: Vec::new(),
                change_count: cc,
            }))
        })
    }

    pub fn write_text(text: &str, self_write_token: &AtomicI64) -> Result<i64> {
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ns = NSString::from_str(text);
            let types = NSArray::from_slice(&[unsafe { NSPasteboardTypeString }]);
            let _ = unsafe { pb.declareTypes_owner(&types, None) };
            let ok = pb.setString_forType(&ns, unsafe { NSPasteboardTypeString });
            if !ok {
                return Err(Error::ClipboardAccess(
                    "NSPasteboard setString:forType: returned NO".into(),
                ));
            }
            let cc = pb.changeCount() as i64;
            self_write_token.store(cc, Ordering::SeqCst);
            Ok(cc)
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn current_change_count() -> i64 {
        0
    }

    pub fn poll_once(
        _self_write_token: &AtomicI64,
        _last: i64,
    ) -> Result<Option<ClipboardEvent>> {
        Err(Error::ClipboardAccess("NSPasteboard requires macOS".into()))
    }

    pub fn write_text(_text: &str, _self_write_token: &AtomicI64) -> Result<i64> {
        Err(Error::ClipboardAccess("NSPasteboard requires macOS".into()))
    }
}

// Re-export for the pipeline (Phase 11) — currently flagged unused
// because no caller exists yet outside of this crate's tests.
#[allow(unused_imports)]
pub use imp::{current_change_count, poll_once, write_text};

#[cfg(test)]
mod tests {
    use super::*;

    /// Live FFI test — verifies the changeCount monotonically advances.
    /// Gated because shared system pasteboard makes this a poor CI
    /// citizen. Run with `cargo test -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches the system pasteboard; run with --ignored"]
    fn change_count_advances_after_write() {
        let token = AtomicI64::new(0);
        let before = current_change_count();
        write_text("textlog test write", &token).expect("write should succeed");
        let after = current_change_count();
        assert!(after > before, "{after} > {before}");
        assert_eq!(token.load(Ordering::SeqCst), after);
    }

    /// Round-trip: write_text then poll_once must short-circuit because
    /// the change is OUR own write.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches the system pasteboard; run with --ignored"]
    fn poll_skips_self_write() {
        let token = AtomicI64::new(0);
        let last = current_change_count();
        let cc = write_text("self-write skip target", &token).unwrap();
        assert!(cc > last, "write must advance the change count");
        let ev = poll_once(&token, last).expect("poll should not error");
        assert!(ev.is_none(), "self-write must short-circuit; got {ev:?}");
    }

    /// poll_once with last >= current returns None even on macOS.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches the system pasteboard; run with --ignored"]
    fn poll_skips_when_last_is_in_future() {
        let token = AtomicI64::new(0);
        let cc = current_change_count();
        let ev = poll_once(&token, cc + 1_000_000).unwrap();
        assert!(ev.is_none());
    }

    #[test]
    fn clipboard_event_carries_change_count() {
        let ev = ClipboardEvent {
            kind: Kind::Text,
            bytes: b"hi".to_vec(),
            change_count: 42,
        };
        assert_eq!(ev.change_count, 42);
        assert_eq!(ev.kind, Kind::Text);
    }
}
