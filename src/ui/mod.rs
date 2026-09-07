//! The desktop widget.

mod theme;
mod tray;
mod widget;

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::config::{self, MachineConfig, Mode, UserPrefs};
use crate::install;
use crate::net::{self, adapters, adapters::Nic, routes::Verdict, wcm, wifi::WifiStatus, Snapshot};

/// Result of a switch, delivered from the worker thread back to the UI.
pub struct ApplyResult {
    pub mode: Mode,
    pub error: Option<String>,
}

/// Everything the widget draws from, read as one consistent unit.
pub struct View {
    pub snap: Snapshot,
    pub eth: Option<Nic>,
    pub wifi: Option<Nic>,
    pub verdict: Verdict,
    pub wifi_status: Option<WifiStatus>,
    pub policy: wcm::PolicyState,
    pub installed: bool,
    /// Ethernet's IP stack is detached -- the "Wi-Fi only" mode is in effect.
    pub eth_ip_detached: bool,
    /// Set when silencing Ethernet's IP stack would still leave frames on the wire.
    pub bridge_note: Option<String>,
    /// The mode LinkSwitch last applied, from the journal. This is what the user *asked for*,
    /// which is a different question from what is currently carrying traffic -- with a VPN in
    /// the way the latter is unknowable, and lighting a button on that basis would claim a
    /// mode the user never chose.
    pub applied_mode: Option<Mode>,
}

impl View {
    fn read(cfg: &MachineConfig) -> Self {
        let snap = Snapshot::read();
        let (eth_c, wifi_c) = snap.candidates();
        let eth = config::resolve(cfg.ethernet.as_ref(), adapters::NicKind::Ethernet, &snap.nics)
            .or_else(|| eth_c.first().copied())
            .cloned();
        let wifi = config::resolve(cfg.wifi.as_ref(), adapters::NicKind::Wifi, &snap.nics)
            .or_else(|| wifi_c.first().copied())
            .cloned();
        let verdict = snap.verdict(eth.as_ref().map(|n| n.luid), wifi.as_ref().map(|n| n.luid));
        let (eth_ip_detached, bridge_note) = match eth.as_ref() {
            Some(n) => (
                net::binding::read(&n.adapter_name)
                    .map(|b| !b.any())
                    .unwrap_or(false),
                net::binding::bridge_warning(&n.adapter_name),
            ),
            None => (false, None),
        };
        let journal = config::load_journal();
        let applied_mode = (!journal.in_flight && journal.finished_unix.is_some())
            .then_some(journal.mode);

        Self {
            snap,
            eth,
            wifi,
            verdict,
            wifi_status: net::wifi::status(),
            policy: wcm::effective(),
            installed: install::is_installed(),
            eth_ip_detached,
            bridge_note,
            applied_mode,
        }
    }

    /// Is Ethernet currently switched off, in the user's sense of "as if unplugged"?
    ///
    /// True for either flavour of off: the IP stack detached, or Ethernet demoted so Wi-Fi
    /// carries the traffic.
    pub fn ethernet_is_off(&self) -> bool {
        self.eth_ip_detached || self.active_mode() == Some(Mode::Wifi)
    }

    /// Which mode is in effect, for highlighting a button.
    ///
    /// Live state first where it is unambiguous, then the journal. Deliberately returns `None`
    /// rather than guessing: when a VPN holds the default route there is no honest answer, and
    /// lighting "Auto" in that situation would assert a mode the user never selected.
    pub fn active_mode(&self) -> Option<Mode> {
        // A detached IP stack is definitive, and outranks the routing table -- which cannot
        // tell it apart from an unplugged cable.
        if self.eth_ip_detached {
            return Some(Mode::WifiOnly);
        }
        match self.verdict {
            Verdict::Ethernet { .. } => Some(Mode::Ethernet),
            Verdict::Wifi { .. } => Some(Mode::Wifi),
            // Hijacked or None: fall back to what we last applied, if anything.
            _ => self.applied_mode,
        }
    }
}

pub struct App {
    pub cfg: MachineConfig,
    pub prefs: UserPrefs,
    pub view: View,
    /// A switch is in flight; buttons are disabled and the UI polls for the outcome.
    pub pending: Option<(Mode, Instant)>,
    pub status: Option<String>,
    pub error: Option<String>,
    results: Receiver<ApplyResult>,
    sender: Sender<ApplyResult>,
    notify: Option<net::notify::Handles>,
    tray: Option<tray::Tray>,
    visible: bool,
    hwnd_done: bool,
    logged_metrics: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = config::load_machine();
        let prefs = config::load_prefs();
        let view = View::read(&cfg);
        let (sender, results) = channel();
        let notify = net::notify::register(cc.egui_ctx.clone());
        let tray = tray::Tray::new(cc.egui_ctx.clone());
        // A missing tray icon is invisible by definition, and the widget's hide button would
        // then strand the user with a running process they cannot get back to. Record it.
        if tray.is_none() {
            crate::log::line(
                "WARNING: could not create the notification-area icon; the hide button is disabled",
            );
        }
        if notify.is_none() {
            crate::log::line("WARNING: change notifications unavailable; falling back to polling");
        }
        crate::log::line(&format!(
            "widget ready: tray={} notifications={} installed={}",
            tray.is_some(),
            notify.is_some(),
            view.installed
        ));
        theme::apply(&cc.egui_ctx);
        Self {
            cfg,
            prefs,
            view,
            pending: None,
            status: None,
            error: None,
            results,
            sender,
            notify,
            tray,
            visible: true,
            hwnd_done: false,
            logged_metrics: false,
        }
    }

    /// Ask the worker to switch. Runs off the UI thread: starting a scheduled task goes through
    /// COM and can block for a noticeable moment, which would stutter the window.
    pub fn request(&mut self, mode: Mode) {
        if self.pending.is_some() {
            return;
        }
        self.pending = Some((mode, Instant::now()));
        self.error = None;
        self.status = Some(match mode {
            Mode::Wifi => "Connecting Wi-Fi and switching...".into(),
            Mode::WifiOnly => "Detaching Ethernet's IP stack...".into(),
            Mode::Ethernet => "Switching to Ethernet...".into(),
            Mode::Auto => "Restoring automatic metrics...".into(),
        });
        let tx = self.sender.clone();
        std::thread::spawn(move || {
            let error = install::request_apply(mode).err();
            let _ = tx.send(ApplyResult { mode, error });
        });
    }

    /// Can the window be hidden and got back again?
    pub fn can_hide(&self) -> bool {
        self.tray.is_some()
    }

    /// Handle notification-area clicks and keep the tooltip current.
    fn pump_tray(&mut self, ctx: &egui::Context) {
        while let Some(action) = self.tray.as_ref().and_then(|t| t.poll()) {
            match action {
                tray::TrayAction::Toggle => {
                    self.visible = !self.visible;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(self.visible));
                    if self.visible {
                        // Visible(true) goes through SW_SHOWNOACTIVATE, so focus has to be asked
                        // for separately or the window comes back behind whatever was in front.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        net::notify::mark_stale();
                    }
                }
            }
        }
        if let Some(t) = self.tray.as_mut() {
            t.update_tooltip(&self.view.verdict, self.pending.map(|(m, _)| m));
        }
    }

    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(r) = self.results.try_recv() {
            self.pending = None;
            match r.error {
                Some(e) => {
                    self.error = Some(e);
                    self.status = None;
                }
                None => {
                    self.status = None;
                    // The worker's change may not be visible in the routing table for a moment.
                    net::notify::mark_stale();
                    ctx.request_repaint_after(Duration::from_millis(500));
                }
            }
        }

        // A switch that never reports back must not disable the buttons forever.
        if let Some((mode, started)) = self.pending {
            if started.elapsed() > Duration::from_secs(45) {
                self.pending = None;
                self.status = None;
                self.error = Some(format!(
                    "The {} switch did not report back. Check `linkswitch --status`.",
                    mode.as_str()
                ));
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }

        if net::notify::take_dirty() {
            self.view = View::read(&self.cfg);
        }
    }
}

impl eframe::App for App {
    /// Real transparency needs this as well as the viewport flag; the flag alone still paints an
    /// opaque background.
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    /// Keeps running while the window is hidden in the tray, which is what lets a tray click
    /// bring it back.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.pump(ctx);
        self.pump_tray(ctx);
        if !self.hwnd_done {
            self.hwnd_done = true;
            crate::ui::widget::polish_window(frame);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // This `Ui` has no background and no margin of its own; without an explicit frame the
        // result on a transparent window is an unpainted void that looks exactly like a broken
        // renderer.
        let panel = egui::Frame::new()
            .fill(theme::CARD)
            .corner_radius(12.0)
            .inner_margin(egui::Margin::symmetric(14, 12))
            .stroke(egui::Stroke::new(1.0, theme::BORDER));
        egui::CentralPanel::default()
            .frame(panel)
            .show(ui, |ui| {
                if !self.logged_metrics {
                    self.logged_metrics = true;
                    let r = ui.max_rect();
                    crate::log::line(&format!(
                        "layout: pixels_per_point={:.2} usable={}x{} points",
                        ui.ctx().pixels_per_point(),
                        r.width().round(),
                        r.height().round()
                    ));
                }
                widget::draw(self, ui)
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Only safe to cancel from here: doing it inside a callback is a documented deadlock.
        net::notify::unregister(self.notify.take());
        let _ = config::save_prefs(&self.prefs);
    }
}

pub fn run() -> Result<(), String> {
    let prefs = config::load_prefs();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("LinkSwitch")
        .with_inner_size([348.0, 262.0])
        .with_min_inner_size([348.0, 262.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_resizable(false)
        .with_drag_and_drop(false)
        // Removes the taskbar button. It does NOT remove the Alt-Tab entry -- winit implements
        // this as ITaskbarList::DeleteTab rather than WS_EX_TOOLWINDOW -- so `polish_window`
        // applies the window style afterwards.
        .with_taskbar(false);
    if let Some((x, y)) = prefs.pos {
        viewport = viewport.with_position([x, y]);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "LinkSwitch",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|e| format!("cannot open the widget: {e}"))
}
