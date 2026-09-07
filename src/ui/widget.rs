//! Drawing the widget.

use egui::{Align, Color32, Layout, RichText, Sense, Ui};

use super::theme;
use super::{App, View};
use crate::config::Mode;
use crate::net::adapters::{Nic, NicKind};
use crate::net::routes::Verdict;
use crate::net::wcm::MinimizePolicy;

pub fn draw(app: &mut App, ui: &mut Ui) {
    // The whole card is a drag handle. This must be registered BEFORE the controls: egui gives
    // the interaction to the last widget added, so reversing these two blocks silently stops
    // every button working.
    let body = ui.max_rect();
    let bg = ui.interact(body, egui::Id::new("ls_drag"), Sense::click_and_drag());
    if bg.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
        title_bar(app, ui);
        ui.add_space(6.0);
        link_row(ui, &app.view, NicKind::Ethernet);
        ui.add_space(2.0);
        link_row(ui, &app.view, NicKind::Wifi);
        ui.add_space(9.0);
        buttons(app, ui);
        ui.add_space(7.0);
        banner(app, ui);
    });
}

/// A close cross, drawn rather than typed.
///
/// Every decorative glyph in this widget is painted with primitives. The default egui font has
/// no coverage for "✕", "◉" or the block-drawing bars, and a missing glyph renders as an empty
/// box -- which is exactly what the first build of this window did.
fn close_button(ui: &mut Ui) -> egui::Response {
    let size = egui::vec2(16.0, 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let colour = if resp.hovered() {
        theme::TEXT
    } else {
        theme::MUTED
    };
    let p = ui.painter();
    let c = rect.center();
    let r = 3.5;
    let stroke = egui::Stroke::new(1.4, colour);
    p.line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
    p.line_segment([c + egui::vec2(r, -r), c + egui::vec2(-r, r)], stroke);
    resp
}

/// The winner marker: a ring, filled when this link is the one carrying traffic.
fn marker(ui: &mut Ui, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), Sense::hover());
    let c = rect.center();
    let colour = if filled { theme::ACTIVE } else { theme::IDLE };
    let p = ui.painter();
    p.circle_stroke(c, 5.0, egui::Stroke::new(1.5, colour));
    if filled {
        p.circle_filled(c, 2.6, colour);
    }
}

/// Signal strength as four bars, lit in proportion to quality.
fn signal_bars(ui: &mut Ui, quality: u32) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 12.0), Sense::hover());
    let lit = lit_bars(quality);
    let p = ui.painter();
    for i in 0..4u32 {
        let h = 3.0 + i as f32 * 2.6;
        let x = rect.left() + i as f32 * 4.5;
        let bar = egui::Rect::from_min_max(
            egui::pos2(x, rect.bottom() - h),
            egui::pos2(x + 3.0, rect.bottom()),
        );
        let colour = if i < lit { theme::TEXT } else { theme::IDLE };
        p.rect_filled(bar, 1.0, colour);
    }
    resp.on_hover_text(format!("Signal {quality}%"));
}

/// How many of the four bars are lit. Nonzero whenever there is any signal at all, so a weak but
/// working link never looks identical to no link.
fn lit_bars(quality: u32) -> u32 {
    match quality {
        0 => 0,
        1..=25 => 1,
        26..=50 => 2,
        51..=75 => 3,
        _ => 4,
    }
}

fn title_bar(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("LinkSwitch")
                .color(theme::TEXT)
                .size(13.0)
                .strong(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Offering "hide" with no notification-area icon to restore from would leave the
            // user with a running process and no way to see it again.
            if !app.can_hide() {
                return;
            }
            if close_button(ui)
                .on_hover_text("Hide. LinkSwitch keeps running in the background.")
                .clicked()
            {
                // Never `ViewportCommand::Close`: eframe returns from `run_native` on close,
                // which stops the message pump and takes the tray icon with it, leaving no way
                // to get the window back.
                app.visible = false;
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        });
    });
}

/// One adapter row: name, live state, metric, and a marker showing who is winning.
fn link_row(ui: &mut Ui, view: &View, kind: NicKind) {
    let (nic, label) = match kind {
        NicKind::Ethernet => (view.eth.as_ref(), "Ethernet"),
        _ => (view.wifi.as_ref(), "Wi-Fi"),
    };

    let winning = matches!(
        (&view.verdict, kind),
        (Verdict::Ethernet { .. }, NicKind::Ethernet) | (Verdict::Wifi { .. }, NicKind::Wifi)
    );

    ui.horizontal(|ui| {
        // Filled versus hollow, not just colour: the state has to be readable without relying on
        // hue alone.
        marker(ui, winning);

        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).color(theme::TEXT).size(12.5));
                ui.label(
                    RichText::new(state_text(nic, kind, view))
                        .color(theme::MUTED)
                        .size(11.0),
                );
                // Bars go last so they sit beside the SSID rather than pushing it around.
                if kind == NicKind::Wifi {
                    if let Some(q) = view
                        .wifi_status
                        .as_ref()
                        .filter(|w| w.connected)
                        .and_then(|w| w.signal_quality)
                    {
                        signal_bars(ui, q);
                    }
                }
            });
            ui.label(
                RichText::new(detail_text(nic, kind, view))
                    .color(theme::MUTED)
                    .size(10.5),
            );
        });
    });
}

/// The short status beside the adapter name.
///
/// Distinguishing "cable unplugged" from "parked by LinkSwitch" matters more than it looks: both
/// leave the adapter with no default route, so anything derived only from routing shows them
/// identically -- and the entire promise of this app is that the cable stays plugged in.
fn state_text(nic: Option<&Nic>, kind: NicKind, view: &View) -> String {
    let Some(n) = nic else {
        return "not found".into();
    };
    match kind {
        NicKind::Ethernet => {
            if view.eth_ip_detached {
                // Distinct from "cable unplugged" on purpose: both leave the adapter with no
                // routes, and conflating them would hide the whole point of this mode.
                "IP stack detached".into()
            } else if !n.media_connected {
                "cable unplugged".into()
            } else if !n.oper_up {
                "down".into()
            } else {
                n.link_speed_text().unwrap_or_else(|| "connected".into())
            }
        }
        _ => match view.wifi_status.as_ref() {
            Some(w) if w.connected => w.ssid.clone().unwrap_or_else(|| "connected".into()),
            Some(w) if !w.radio_on => "radio off".into(),
            _ => "not connected".into(),
        },
    }
}

fn detail_text(nic: Option<&Nic>, kind: NicKind, _view: &View) -> String {
    let Some(n) = nic else {
        return String::new();
    };
    let ip = n
        .ipv4
        .iter()
        .find(|a| !a.is_link_local())
        .map(|a| a.to_string());
    let m = crate::net::metric::read(n.luid, windows::Win32::Networking::WinSock::AF_INET).ok();
    let metric = match m {
        Some(s) if s.automatic => format!("metric {} (auto)", s.metric),
        Some(s) => format!("metric {} (set by LinkSwitch)", s.metric),
        None => "no IPv4".into(),
    };
    let _ = kind;
    match ip {
        Some(ip) => format!("{ip} · {metric}"),
        None => metric,
    }
}

fn buttons(app: &mut App, ui: &mut Ui) {
    let busy = app.pending.is_some();
    let active = app.view.active_mode();
    let policy_blocks_wifi = !app.view.policy.policy.permits_manual_wifi();
    let eth_unplugged = app
        .view
        .eth
        .as_ref()
        .map(|n| !n.media_connected)
        .unwrap_or(true);
    let eth_detached = app.view.eth_ip_detached;

    // Two rows of two rather than four across: at 330 px, four buttons leave no room for
    // "Wi-Fi only" to be readable, and truncating the one mode that needs explaining is the
    // wrong trade.
    let w = (ui.available_width() - 8.0) / 2.0;

    let mut button = |ui: &mut Ui, mode: Mode, text: &str, on: bool, enabled: bool, tip: &str| {
        let fill = if on { theme::BTN_ON } else { theme::BTN };
        let resp = ui.add_enabled(
            enabled && !busy,
            egui::Button::new(RichText::new(text).color(theme::TEXT).size(12.0))
                .fill(fill)
                .min_size(egui::vec2(w, 26.0))
                .corner_radius(7.0),
        );
        let resp = resp.on_disabled_hover_text(tip.to_string());
        if resp.on_hover_text(tip.to_string()).clicked() {
            app.request(mode);
        }
    };

    ui.horizontal(|ui| {
        button(
            ui,
            Mode::Ethernet,
            "Ethernet",
            active == Some(Mode::Ethernet) && !eth_detached,
            !eth_unplugged,
            if eth_unplugged {
                "The Ethernet cable is not connected."
            } else {
                "Send traffic over the cable."
            },
        );
        button(
            ui,
            Mode::Wifi,
            "Wi-Fi",
            active == Some(Mode::Wifi) && !eth_detached,
            !policy_blocks_wifi,
            if policy_blocks_wifi {
                "Group Policy prevents Wi-Fi while Ethernet is connected."
            } else {
                "Send traffic over Wi-Fi. Ethernet stays connected and keeps its own subnet."
            },
        );
    });
    ui.horizontal(|ui| {
        button(
            ui,
            Mode::WifiOnly,
            "Wi-Fi only",
            eth_detached,
            !policy_blocks_wifi,
            if policy_blocks_wifi {
                "Group Policy prevents Wi-Fi while Ethernet is connected."
            } else {
                "Detach Ethernet's IP stack entirely: no address, no routes, no DNS. The cable                  stays plugged in and the adapter stays enabled."
            },
        );
        button(
            ui,
            Mode::Auto,
            "Auto",
            active == Some(Mode::Auto),
            true,
            "Let Windows decide again.",
        );
    });
}

/// The one line under the buttons. Only the most important thing is shown, in priority order.
fn banner(app: &App, ui: &mut Ui) {
    let (text, colour) = banner_text(app);
    ui.label(RichText::new(text).color(colour).size(10.5));
}

fn banner_text(app: &App) -> (String, Color32) {
    if let Some(e) = &app.error {
        return (e.clone(), theme::DANGER);
    }
    if let Some(s) = &app.status {
        return (s.clone(), theme::WARN);
    }
    if !app.view.installed {
        return (
            "Not installed yet — run `linkswitch --install` once to switch without a UAC prompt."
                .into(),
            theme::WARN,
        );
    }
    // A VPN owning the default route is normal, but the user has to know that switching will not
    // change their public IP, or they will report the app as broken.
    if let Verdict::Hijacked { luid, if_index, .. } = &app.view.verdict {
        let name = app
            .view
            .snap
            .nic(*luid)
            .map(|n| n.label().to_string())
            .unwrap_or_else(|| format!("interface {if_index}"));
        return (
            format!("{name} is carrying all traffic. LinkSwitch chooses which link it runs over."),
            theme::WARN,
        );
    }
    if app.view.policy.policy == MinimizePolicy::PreventWifi {
        return (
            "Group Policy prevents Wi-Fi while Ethernet is connected.".into(),
            theme::DANGER,
        );
    }
    if app.view.eth_ip_detached {
        // Never let this mode read as "the wire is silent". It is not.
        return match &app.view.bridge_note {
            Some(note) => (
                format!("Ethernet has no IP stack. {note}"),
                theme::WARN,
            ),
            None => (
                "Ethernet has no IP stack. Discovery protocols still reach the wire.".into(),
                theme::MUTED,
            ),
        };
    }
    if matches!(app.view.verdict, Verdict::None) {
        return ("No network route right now.".into(), theme::DANGER);
    }
    (
        "Applies to new connections. DNS lookups still go out on both links.".into(),
        theme::MUTED,
    )
}

/// One-time window tweaks that egui does not expose.
///
/// `with_taskbar(false)` removes the taskbar button but leaves the Alt-Tab entry, because winit
/// implements it as `ITaskbarList::DeleteTab` rather than a window style. A widget should be in
/// neither, so the tool-window style is applied directly once the window exists.
pub fn polish_window(frame: &eframe::Frame) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_APPWINDOW,
        WS_EX_TOOLWINDOW,
    };

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
    // SAFETY: `hwnd` is a live window handle owned by this process, and the style bits are the
    // documented values for GWL_EXSTYLE.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let next = (ex | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next as isize);

        // Mandatory follow-up. A GWL_EXSTYLE change is not committed until a SetWindowPos with
        // SWP_FRAMECHANGED, and without it the window also silently loses its topmost Z-order --
        // observed directly: the widget carried on running, tray and all, quietly behind the
        // editor. HWND_TOPMOST re-asserts what the ViewportBuilder asked for.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lit_bars_climb_with_quality_and_never_exceed_four() {
        assert_eq!(lit_bars(0), 0);
        assert_eq!(lit_bars(1), 1);
        assert_eq!(lit_bars(50), 2);
        assert_eq!(lit_bars(85), 4);
        assert_eq!(lit_bars(100), 4);
        for q in 0..=100 {
            assert!(lit_bars(q) <= 4, "quality {q} lit too many bars");
        }
    }

    #[test]
    fn lit_bars_is_monotonic() {
        // A stronger signal must never show fewer bars than a weaker one.
        let mut prev = 0;
        for q in 0..=100 {
            let n = lit_bars(q);
            assert!(n >= prev, "quality {q} went backwards");
            prev = n;
        }
    }

    #[test]
    fn any_signal_at_all_lights_a_bar() {
        // A weak but working link must not look identical to no link.
        assert!(lit_bars(1) > 0);
    }
}
