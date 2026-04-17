//! Best-effort macOS permission probes used by `tl doctor`.
//!
//! macOS's permission APIs are TCC-driven: a real "did the user grant
//! this prompt?" answer requires bundling, an Info.plist, and (for
//! notifications) `UNUserNotificationCenter` callbacks. For an
//! unsigned CLI the cheapest signal is to **try the operation** and
//! report whether it worked.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    /// Couldn't determine — surface as informational rather than fail.
    Unknown,
}

/// Probe pasteboard access by reading `changeCount`. macOS may pop a
/// privacy banner on first read in 15.4+, but the call still succeeds
/// — so a non-panicking read is our "Granted" signal.
#[cfg(target_os = "macos")]
pub fn pasteboard_access_state() -> PermissionState {
    use std::panic::AssertUnwindSafe;

    let probed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let cc = crate::clipboard::current_change_count();
        // -1 is a sentinel some Apple APIs return on failure; treat as denied.
        cc >= 0
    }));
    match probed {
        Ok(true) => PermissionState::Granted,
        Ok(false) => PermissionState::Denied,
        Err(_) => PermissionState::Unknown,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pasteboard_access_state() -> PermissionState {
    PermissionState::Unknown
}

/// Notification permission can't be queried synchronously without
/// UNUserNotificationCenter callbacks (and even then it requires the
/// app to be bundled). Surface as Unknown — `tl doctor` reports it as
/// informational.
pub fn notification_state() -> PermissionState {
    PermissionState::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasteboard_state_returns_a_value() {
        // We can't assert Granted vs Denied generally — assert no panic
        // and that we got one of the three states.
        let s = pasteboard_access_state();
        assert!(matches!(
            s,
            PermissionState::Granted | PermissionState::Denied | PermissionState::Unknown
        ));
    }

    #[test]
    fn notification_state_is_unknown_until_we_have_a_real_probe() {
        assert_eq!(notification_state(), PermissionState::Unknown);
    }
}
