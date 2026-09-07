//! The notification-area icon.
//!
//! Not optional. The widget's ✕ hides the window rather than closing it -- eframe returns from
//! `run_native` on a real close, which stops the message pump and takes the tray icon down with
//! it -- so without a tray icon, hiding the window would strand the user with a running process
//! and no way to see it again.

use std::sync::mpsc::{channel, Receiver};

use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::Mode;
use crate::net::routes::Verdict;

/// What the user did in the notification area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Show the widget if hidden, hide it if visible.
    Toggle,
}

pub struct Tray {
    /// Dropping this destroys the icon, so it must be kept alive for the life of the app.
    icon: TrayIcon,
    events: Receiver<TrayAction>,
    current_tip: String,
}

impl Tray {
    /// Build the tray icon.
    ///
    /// Must be called on the thread that pumps messages -- which is why this happens inside
    /// eframe's app-creator closure rather than before `run_native`.
    pub fn new(ctx: egui::Context) -> Option<Self> {
        // Resource id 1 is the icon embedded by build.rs. Falling back to a generated bitmap
        // keeps a `cargo run` without the resource from silently having no tray icon.
        let icon = Icon::from_resource(1, None).ok().or_else(fallback_icon)?;

        let (tx, events) = channel();

        // Push, not poll. A correctly reactive widget sits blocked in `ControlFlow::Wait`, so it
        // would never get around to draining a polled receiver -- the click would appear to do
        // nothing until something else happened to wake the loop.
        //
        // `set_event_handler` is backed by a `OnceLock`: it can be set exactly once, and once
        // set, `TrayIconEvent::receiver()` yields nothing.
        TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
            if let TrayIconEvent::Click { button, .. } = ev {
                if button == tray_icon::MouseButton::Left {
                    let _ = tx.send(TrayAction::Toggle);
                    ctx.request_repaint();
                }
            }
        }));

        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("LinkSwitch")
            .build()
            .ok()?;

        Some(Self {
            icon: tray,
            events,
            current_tip: String::new(),
        })
    }

    pub fn poll(&self) -> Option<TrayAction> {
        self.events.try_recv().ok()
    }

    /// Keep the tooltip in step with reality.
    ///
    /// The whole point of a tray app is being able to tell what it is doing without opening it,
    /// so a static "LinkSwitch" tooltip would waste the only glanceable surface there is.
    pub fn update_tooltip(&mut self, verdict: &Verdict, mode: Option<Mode>) {
        let tip = tooltip_for(verdict, mode);
        if tip != self.current_tip {
            let _ = self.icon.set_tooltip(Some(&tip));
            self.current_tip = tip;
        }
    }
}

pub fn tooltip_for(verdict: &Verdict, pending: Option<Mode>) -> String {
    if let Some(m) = pending {
        return format!("LinkSwitch — switching to {}...", m.as_str());
    }
    match verdict {
        Verdict::Ethernet { .. } => "LinkSwitch — traffic is going over Ethernet".into(),
        Verdict::Wifi { .. } => "LinkSwitch — traffic is going over Wi-Fi".into(),
        Verdict::Hijacked { .. } => "LinkSwitch — a VPN is carrying all traffic".into(),
        Verdict::None => "LinkSwitch — no network route".into(),
    }
}

/// A plain 16x16 mark, used only when the embedded resource is unavailable.
fn fallback_icon() -> Option<Icon> {
    const N: u32 = 16;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let inside = (4..12).contains(&y) && (2..14).contains(&x);
            if inside {
                rgba.extend_from_slice(&[0x4C, 0xC2, 0x8C, 0xFF]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, N, N).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::LuidKey;

    #[test]
    fn the_tooltip_says_which_link_is_carrying_traffic() {
        assert!(tooltip_for(&Verdict::Ethernet { total: 5 }, None).contains("Ethernet"));
        assert!(tooltip_for(&Verdict::Wifi { total: 30 }, None).contains("Wi-Fi"));
        assert!(tooltip_for(
            &Verdict::Hijacked {
                luid: LuidKey(1),
                if_index: 43,
                total: 0
            },
            None
        )
        .contains("VPN"));
        assert!(tooltip_for(&Verdict::None, None).contains("no network"));
    }

    #[test]
    fn a_pending_switch_takes_over_the_tooltip() {
        let t = tooltip_for(&Verdict::Ethernet { total: 5 }, Some(Mode::Wifi));
        assert!(t.contains("switching"), "got {t}");
        assert!(t.contains("wifi"), "got {t}");
    }

    #[test]
    fn every_tooltip_is_prefixed_so_it_is_identifiable_in_a_crowded_tray() {
        for v in [
            Verdict::Ethernet { total: 5 },
            Verdict::Wifi { total: 30 },
            Verdict::None,
        ] {
            assert!(tooltip_for(&v, None).starts_with("LinkSwitch"));
        }
    }

    #[test]
    fn the_fallback_icon_is_a_valid_bitmap() {
        assert!(fallback_icon().is_some());
    }
}
