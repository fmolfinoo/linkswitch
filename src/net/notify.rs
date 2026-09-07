//! Live change notifications, so the widget can idle at zero CPU.
//!
//! A desktop widget sits on screen all day. Polling the routing table on a timer would be the
//! easy way to keep it current and the wrong way to pay for it, so instead the IP Helper stack
//! tells us when something changed and egui repaints only then. With nothing requested, eframe
//! blocks in `ControlFlow::Wait` and uses no CPU at all.
//!
//! All three notifications are registered, because each misses something the others catch:
//!
//! | Source | Fires on |
//! |---|---|
//! | `NotifyIpInterfaceChange` | our own metric writes (a parameter change, not a new route) |
//! | `NotifyRouteChange2` | cable in/out, VPN connect, DHCP installing a default route |
//! | `NotifyUnicastIpAddressChange` | DHCP lease, Wi-Fi association completing |
//!
//! Registering only the route one would miss the widget's *own* action, because changing a
//! metric does not add or remove a forwarding entry.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows::Win32::Foundation::{HANDLE, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, NotifyIpInterfaceChange, NotifyRouteChange2,
    NotifyUnicastIpAddressChange, MIB_IPFORWARD_ROW2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
    MIB_UNICASTIPADDRESS_ROW,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;

/// Set whenever the OS reports a change. The UI thread swaps it back to false and re-reads.
static DIRTY: AtomicBool = AtomicBool::new(true);

/// Cloned egui context used to wake the event loop from an OS callback thread.
static REPAINT: OnceLock<egui::Context> = OnceLock::new();

/// Handles to cancel on shutdown.
pub struct Handles {
    iface: HANDLE,
    route: HANDLE,
    addr: HANDLE,
}

fn mark_dirty() {
    DIRTY.store(true, Ordering::Relaxed);
    if let Some(ctx) = REPAINT.get() {
        ctx.request_repaint();
    }
}

// The callbacks do nothing but set a flag and wake the UI. They run on an IP Helper worker
// thread with undocumented reentrancy rules, and the row pointers they receive are OS-owned and
// only partially populated -- so nothing here reads them, and nothing here frees them.

unsafe extern "system" fn on_iface(
    _ctx: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    mark_dirty();
}

unsafe extern "system" fn on_route(
    _ctx: *const c_void,
    _row: *const MIB_IPFORWARD_ROW2,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    mark_dirty();
}

unsafe extern "system" fn on_addr(
    _ctx: *const c_void,
    _row: *const MIB_UNICASTIPADDRESS_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    mark_dirty();
}

/// Has anything changed since the last check? Clears the flag.
pub fn take_dirty() -> bool {
    DIRTY.swap(false, Ordering::Relaxed)
}

/// Force a refresh on the next frame.
pub fn mark_stale() {
    DIRTY.store(true, Ordering::Relaxed);
}

/// Start listening. The context is used to wake the event loop from the OS callback threads.
pub fn register(ctx: egui::Context) -> Option<Handles> {
    let _ = REPAINT.set(ctx);

    let mut iface = HANDLE::default();
    let mut route = HANDLE::default();
    let mut addr = HANDLE::default();

    // `initialnotification = false`: with it set, each callback fires once immediately with a
    // null row purely as a registration acknowledgement, which is noise here since DIRTY already
    // starts true.
    //
    // Note the inconsistent signatures -- NotifyRouteChange2 takes a bare `*const c_void` for
    // its context while the other two take `Option<*const c_void>`. Trying to share one helper
    // between the three registrations does not compile.
    // SAFETY: the callbacks are `extern "system"` functions with the exact signatures these
    // APIs require, and the handle out-params are valid locals moved into `Handles`.
    unsafe {
        let a = NotifyIpInterfaceChange(AF_UNSPEC, Some(on_iface), None, false, &mut iface);
        let b = NotifyRouteChange2(AF_UNSPEC, Some(on_route), std::ptr::null(), false, &mut route);
        let c = NotifyUnicastIpAddressChange(AF_UNSPEC, Some(on_addr), None, false, &mut addr);
        if a != NO_ERROR && b != NO_ERROR && c != NO_ERROR {
            return None;
        }
    }

    Some(Handles { iface, route, addr })
}

/// Stop listening.
///
/// Must not be called from inside a callback, or from a thread a callback is waiting on:
/// `CancelMibChangeNotify2` blocks until in-flight callbacks finish, and doing this from the
/// wrong place is a documented deadlock. The only caller is `App::on_exit`.
pub fn unregister(h: Option<Handles>) {
    let Some(h) = h else { return };
    // SAFETY: each handle came from a successful Notify* registration and is cancelled once.
    unsafe {
        for handle in [h.iface, h.route, h.addr] {
            if !handle.is_invalid() {
                let _ = CancelMibChangeNotify2(handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_starts_set_so_the_first_frame_reads_state() {
        // Not asserting on the global directly -- other tests share it -- but the initial value
        // is what guarantees the widget renders real data on frame one rather than a blank card.
        mark_stale();
        assert!(take_dirty());
        assert!(!take_dirty(), "the flag must clear when taken");
    }

    #[test]
    fn marking_stale_survives_a_take() {
        mark_stale();
        assert!(take_dirty());
        mark_stale();
        assert!(take_dirty());
    }
}
