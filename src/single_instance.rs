//! One widget, one tray icon.
//!
//! Nothing stopped a second copy from starting, and each copy creates its own notification-area
//! icon -- so launching from the Start menu while one was already running left two identical
//! LinkSwitch icons in the tray, with no way to tell them apart.
//!
//! A named mutex settles who owns the session. The second process then signals a named event and
//! exits, and the first process -- which polls that event from `App::logic`, a method eframe keeps
//! ticking even while the window is hidden -- unhides itself. So clicking the shortcut a second
//! time does the useful thing (bring the widget back) instead of the confusing thing (a duplicate
//! icon).
//!
//! Both objects live in the `Local\` namespace, which is per logon session. That is the right
//! scope for a per-user widget: two different users signed in at once each get their own widget,
//! and no privileged `Global\` name is needed.

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, ReleaseMutex, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE, SYNCHRONIZATION_ACCESS_RIGHTS,
};

const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\LinkSwitch.SingleInstance");
const EVENT_NAME: windows::core::PCWSTR = w!("Local\\LinkSwitch.ShowWindow");

/// Held for the lifetime of the owning process.
pub struct InstanceGuard {
    mutex: HANDLE,
    event: HANDLE,
}

impl InstanceGuard {
    /// Has another launch asked us to show ourselves since the last check?
    pub fn show_requested(&self) -> bool {
        if self.event.is_invalid() {
            return false;
        }
        // Zero timeout: a poll, never a block. This runs on the UI thread.
        // SAFETY: `event` is a live auto-reset event handle owned by this struct.
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: both handles were created by `acquire` and are closed exactly once.
        unsafe {
            if !self.mutex.is_invalid() {
                let _ = ReleaseMutex(self.mutex);
                let _ = CloseHandle(self.mutex);
            }
            if !self.event.is_invalid() {
                let _ = CloseHandle(self.event);
            }
        }
    }
}

/// Claim ownership of this session's widget.
///
/// `Some` means we are the only instance. `None` means another one is already running and has
/// been asked to show itself; the caller should exit quietly.
pub fn acquire() -> Option<InstanceGuard> {
    // SAFETY: creating named kernel objects; every handle is either stored in the guard or
    // closed before returning.
    unsafe {
        let Ok(mutex) = CreateMutexW(None, true, MUTEX_NAME) else {
            // Without the mutex we cannot arbitrate, so run rather than refuse to start at all.
            return Some(InstanceGuard {
                mutex: HANDLE::default(),
                event: HANDLE::default(),
            });
        };

        // CreateMutexW still returns a valid handle when the name already exists, so the last
        // error is the only way to tell whether we are the owner.
        if windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            signal_show();
            return None;
        }

        // Auto-reset: waking the first instance consumes the request, so one extra launch does
        // not leave the window permanently pinned open.
        let event = CreateEventW(None, false, false, EVENT_NAME).unwrap_or_default();
        Some(InstanceGuard { mutex, event })
    }
}

/// Ask the already-running instance to show its window.
fn signal_show() {
    // SAFETY: opening an existing named event by name; the handle is closed on every path.
    unsafe {
        if let Ok(h) = OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_MODIFY_STATE.0),
            false,
            EVENT_NAME,
        ) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_acquire_wins_and_the_second_is_refused() {
        let first = acquire().expect("first instance should own the session");
        // A second attempt in-process takes the same path a second launch would.
        assert!(
            acquire().is_none(),
            "a second instance must not be allowed to run"
        );
        // ...and it should have left a show request behind for the owner to pick up.
        assert!(
            first.show_requested(),
            "the refused instance should have asked the owner to show itself"
        );
        // Auto-reset: the request is consumed once observed.
        assert!(!first.show_requested());
        drop(first);

        // Once the owner exits, the session is claimable again.
        let again = acquire();
        assert!(again.is_some(), "the guard must be released on drop");
    }
}
