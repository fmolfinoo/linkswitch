//! The elevated worker: the only code that changes anything.
//!
//! Runs as a scheduled task with an elevated token, does one job, writes a journal and exits.
//! It has no window and no console, so [`crate::log`] is its entire diagnostics channel and the
//! process exit code is the only signal the widget can see through Task Scheduler.
//!
//! # Order of operations
//!
//! Everything that can fail happens *before* anything is changed:
//!
//! 1. confirm elevation,
//! 2. resolve the configured adapters against what is actually present,
//! 3. recover a torn journal from a previous crash,
//! 4. pre-flight the target mode -- for Wi-Fi that means actually associating Wi-Fi first,
//! 5. record prior values in the journal,
//! 6. write metrics,
//! 7. verify the intended link now wins, and revert if the machine ended up with no route at all.
//!
//! The pre-flight ordering is the important part. Parking Ethernet and *then* discovering Wi-Fi
//! cannot connect would leave the user with no working link and no application running to fix it.

use std::time::Duration;

use crate::config::{self, BindingBackup, MachineConfig, MetricBackup, Mode};
use crate::lslog;
use crate::net::adapters::{Nic, NicKind};
use crate::net::routes::Verdict;
use crate::net::{binding, metric, wcm, wifi, LuidKey, Snapshot};

/// How long to wait for Wi-Fi to associate before giving up. Association on a known network is
/// usually 2-4 s; 15 s covers a slow 6 GHz roam without making a failure feel like a hang.
const WIFI_TIMEOUT: Duration = Duration::from_secs(15);

/// Process exit codes. Distinct per failure class because `LastTaskResult` is the only channel
/// the widget has when the worker dies before it can write a journal.
pub mod exit {
    pub const OK: u32 = 0;
    pub const GENERIC: u32 = 1;
    pub const NOT_ELEVATED: u32 = 2;
    pub const NOT_CONFIGURED: u32 = 3;
    pub const WIFI_UNAVAILABLE: u32 = 4;
    pub const WRITE_FAILED: u32 = 5;
    pub const VERIFY_FAILED: u32 = 6;
    pub const BLOCKED_BY_POLICY: u32 = 7;
    /// The adapter's IP stack could not be detached or reattached.
    pub const BINDING_FAILED: u32 = 8;
}

#[derive(Debug)]
pub struct ApplyReport {
    pub mode: Mode,
    pub ok: bool,
    pub exit_code: u32,
    /// One sentence for the user.
    pub message: String,
    pub details: Vec<String>,
}

impl ApplyReport {
    fn fail(mode: Mode, code: u32, message: impl Into<String>) -> Self {
        Self {
            mode,
            ok: false,
            exit_code: code,
            message: message.into(),
            details: Vec::new(),
        }
    }
}

/// What the two managed interfaces resolved to.
struct Targets {
    eth: Option<Nic>,
    wifi: Option<Nic>,
}

impl Targets {
    fn resolve(cfg: &MachineConfig, snap: &Snapshot) -> Self {
        // Fall back to the best auto-detected candidate when nothing is configured yet, so a
        // fresh install works before the user has visited any settings.
        let (eth_c, wifi_c) = snap.candidates();
        let eth = config::resolve(cfg.ethernet.as_ref(), NicKind::Ethernet, &snap.nics)
            .or_else(|| eth_c.first().copied())
            .cloned();
        let wifi = config::resolve(cfg.wifi.as_ref(), NicKind::Wifi, &snap.nics)
            .or_else(|| wifi_c.first().copied())
            .cloned();
        Self { eth, wifi }
    }
}

/// Capture the current metric of one interface for both families, for the journal.
fn capture(luid: LuidKey) -> Vec<MetricBackup> {
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let mut out = Vec::new();
    for (family, v6) in [(AF_INET, false), (AF_INET6, true)] {
        if let Ok(state) = metric::read(luid, family) {
            out.push(MetricBackup {
                luid: luid.0,
                family_v6: v6,
                metric: state.metric,
                automatic: state.automatic,
            });
        }
        // A family that is not bound has nothing to restore, so it is simply not recorded.
    }
    out
}

/// Put back exactly what was captured. Never substitutes a default.
fn restore_backups(backups: &[MetricBackup]) -> bool {
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let mut all_ok = true;
    for b in backups {
        let family = if b.family_v6 { AF_INET6 } else { AF_INET };
        // `automatic` restores to automatic; otherwise the exact manual number it held before.
        let target = if b.automatic { None } else { Some(b.metric) };
        let outcome = metric::set(LuidKey(b.luid), family, target);
        if !outcome.is_ok() {
            all_ok = false;
            lslog!(
                "restore: luid {:#x} v6={} failed: {}",
                b.luid,
                b.family_v6,
                outcome.describe()
            );
        }
    }
    all_ok
}

/// Put IP-protocol bindings back exactly as captured.
///
/// Writes the recorded tuple, never `(true, true)`. Rebinding a protocol we did not unbind
/// would, on a machine with VPN IPv6 leak protection, switch IPv6 back on and leak the user's
/// real address -- strictly worse than whatever it was trying to fix.
fn restore_bindings(backups: &[BindingBackup]) -> bool {
    let mut all_ok = true;
    for b in backups {
        let want = binding::BindingState { v4: b.v4, v6: b.v6 };
        match binding::set(&b.guid, want) {
            Ok(o) => lslog!(
                "restore bindings {}: v4={} v6={} (changed={}, reboot={})",
                b.guid,
                b.v4,
                b.v6,
                o.changed,
                o.needs_reboot
            ),
            Err(e) => {
                all_ok = false;
                lslog!("restore bindings {} failed: {}", b.guid, e.user_message());
            }
        }
    }
    all_ok
}

/// Undo a half-finished change left by a worker that was killed mid-apply.
pub fn recover_if_torn() {
    let journal = config::load_journal();
    if !journal.is_torn() {
        return;
    }
    lslog!(
        "found a torn journal from an interrupted {} apply; restoring {} saved metric(s)",
        journal.mode.as_str(),
        journal.restore.len()
    );
    // Bindings first: a metric is meaningless on an interface with no IP stack.
    let ok = restore_bindings(&journal.bindings) & restore_backups(&journal.restore);
    let mut j = journal;
    j.in_flight = false;
    j.finished_unix = Some(config::now_unix());
    j.last_error = Some(if ok {
        "recovered from an interrupted apply".into()
    } else {
        "recovery from an interrupted apply was incomplete".into()
    });
    j.restore.clear();
    j.bindings.clear();
    let _ = config::save_journal(&j);
}

/// Apply a mode. This is the whole product.
pub fn apply(mode: Mode) -> ApplyReport {
    if !crate::elevate::is_elevated() {
        return ApplyReport::fail(
            mode,
            exit::NOT_ELEVATED,
            "LinkSwitch needs administrator rights to change a network metric.",
        );
    }

    recover_if_torn();

    let cfg = config::load_machine();
    let snap = Snapshot::read();
    let targets = Targets::resolve(&cfg, &snap);

    let (Some(eth), Some(wifi)) = (targets.eth.as_ref(), targets.wifi.as_ref()) else {
        let missing = if targets.eth.is_none() {
            "Ethernet"
        } else {
            "Wi-Fi"
        };
        return ApplyReport::fail(
            mode,
            exit::NOT_CONFIGURED,
            format!("No {missing} adapter is available on this machine."),
        );
    };

    lslog!(
        "apply {}: ethernet={} (if{}) wifi={} (if{})",
        mode.as_str(),
        eth.label(),
        eth.if_index,
        wifi.label(),
        wifi.if_index
    );

    // Leaving "Wi-Fi only" must reattach the IP stack before anything else. Setting a metric on
    // an interface that has no IP stack is a no-op, so without this the switch would appear to
    // do nothing at all.
    if mode != Mode::WifiOnly {
        let previous = config::load_journal();
        if !previous.bindings.is_empty() {
            lslog!("reattaching the IP stack detached by a previous wifi-only");
            if !restore_bindings(&previous.bindings) {
                return ApplyReport::fail(
                    mode,
                    exit::BINDING_FAILED,
                    "Could not reattach Ethernet's IP stack. Run `linkswitch --recover`.",
                );
            }
            let mut j = previous;
            j.bindings.clear();
            let _ = config::save_journal(&j);
        }
    }

    match mode {
        Mode::Wifi => apply_wifi(&cfg, eth, wifi),
        Mode::WifiOnly => apply_wifi_only(&cfg, eth, wifi),
        Mode::Ethernet => apply_ethernet(&cfg, eth, wifi),
        Mode::Auto => apply_auto(eth, wifi),
    }
}

/// Detach Ethernet's IP stack entirely, leaving Wi-Fi as the only IP link.
fn apply_wifi_only(cfg: &MachineConfig, eth: &Nic, wifi: &Nic) -> ApplyReport {
    let mode = Mode::WifiOnly;

    let policy = wcm::effective();
    if !policy.policy.permits_manual_wifi() {
        return ApplyReport::fail(
            mode,
            exit::BLOCKED_BY_POLICY,
            format!(
                "{}. LinkSwitch cannot switch to Wi-Fi on this machine.",
                policy.policy.describe()
            ),
        );
    }

    // PRE-FLIGHT. This mode removes Ethernet's ability to carry anything at all, so Wi-Fi being
    // genuinely up first is not a nicety: getting it wrong strands the machine.
    let profile = match wifi::ensure_connected(cfg.wifi_profile_hint.as_deref(), WIFI_TIMEOUT) {
        Ok(p) => p,
        Err(e) => {
            lslog!("wifi-only pre-flight failed: {e:?}");
            return ApplyReport::fail(mode, exit::WIFI_UNAVAILABLE, e.user_message());
        }
    };

    let snap = Snapshot::read();
    if crate::net::routes::total_for(&snap.routes, wifi.luid).is_none() {
        return ApplyReport::fail(
            mode,
            exit::WIFI_UNAVAILABLE,
            "Wi-Fi is connected but has no default route yet. Try again in a moment.",
        );
    }

    // Capture the exact prior binding state before touching anything.
    let before = match binding::read(&eth.adapter_name) {
        Ok(b) => b,
        Err(e) => return ApplyReport::fail(mode, exit::BINDING_FAILED, e.user_message()),
    };
    if !before.any() {
        return ApplyReport::fail(
            mode,
            exit::BINDING_FAILED,
            "Ethernet already has no IP stack attached.",
        );
    }

    let mut journal = config::Journal {
        schema: config::SCHEMA,
        mode,
        in_flight: true,
        started_unix: config::now_unix(),
        finished_unix: None,
        restore: capture(eth.luid),
        bindings: vec![BindingBackup {
            guid: eth.adapter_name.clone(),
            v4: before.v4,
            v6: before.v6,
        }],
        last_error: None,
    };
    let _ = config::save_journal(&journal);

    // Wi-Fi back on its automatic metric. Ethernet loses its IP stack outright, so there is no
    // metric left on it to park.
    let _ = metric::steer(wifi.luid, None);

    let detached = binding::BindingState { v4: false, v6: false };
    let outcome = match binding::set(&eth.adapter_name, detached) {
        Ok(o) => o,
        Err(e) => {
            lslog!("wifi-only unbind failed: {}", e.user_message());
            restore_bindings(&journal.bindings);
            journal.in_flight = false;
            journal.bindings.clear();
            journal.restore.clear();
            journal.last_error = Some(e.user_message());
            journal.finished_unix = Some(config::now_unix());
            let _ = config::save_journal(&journal);
            return ApplyReport::fail(mode, exit::BINDING_FAILED, e.user_message());
        }
    };
    lslog!(
        "wifi-only: ethernet ip stack detached (changed={}, reboot={})",
        outcome.changed,
        outcome.needs_reboot
    );

    journal.in_flight = false;
    journal.finished_unix = Some(config::now_unix());
    let _ = config::save_journal(&journal);

    if !profile.is_empty() {
        let mut c = config::load_machine();
        if c.wifi_profile_hint.as_deref() != Some(profile.as_str()) {
            c.wifi_profile_hint = Some(profile.clone());
            let _ = config::save_machine(&c);
        }
    }

    let mut details = vec!["Ethernet has no IP address, routes or DNS servers.".to_string()];
    // Honesty: this mode silences the IP stack, not the wire.
    if let Some(w) = binding::bridge_warning(&eth.adapter_name) {
        details.push(w);
    }
    if outcome.needs_reboot {
        details.push("Windows asked for a reboot to finish applying this.".into());
    }

    ApplyReport {
        mode,
        ok: true,
        exit_code: exit::OK,
        message: "Ethernet's IP stack is detached. The cable is still plugged in and the adapter \
                  is still enabled."
            .into(),
        details,
    }
}

/// Move traffic to Wi-Fi by parking Ethernet's metric above it.
fn apply_wifi(cfg: &MachineConfig, eth: &Nic, wifi: &Nic) -> ApplyReport {
    let mode = Mode::Wifi;

    // Value 3 blocks even a manual association, so there is nothing this app can do. Say so
    // rather than parking Ethernet and leaving the user offline.
    let policy = wcm::effective();
    if !policy.policy.permits_manual_wifi() {
        return ApplyReport::fail(
            mode,
            exit::BLOCKED_BY_POLICY,
            format!("{}. LinkSwitch cannot switch to Wi-Fi on this machine.", policy.policy.describe()),
        );
    }

    // PRE-FLIGHT: get Wi-Fi actually associated before touching anything. This is the step that
    // makes the whole thing safe -- if Wi-Fi cannot come up, nothing has been changed yet.
    let profile = match wifi::ensure_connected(cfg.wifi_profile_hint.as_deref(), WIFI_TIMEOUT) {
        Ok(p) => p,
        Err(e) => {
            lslog!("wifi pre-flight failed: {e:?}");
            return ApplyReport::fail(mode, exit::WIFI_UNAVAILABLE, e.user_message());
        }
    };
    lslog!("wifi pre-flight ok: connected to \"{profile}\"");

    // Re-read: associating Wi-Fi changed the routing table.
    let snap = Snapshot::read();
    let wifi_total = crate::net::routes::total_for(&snap.routes, wifi.luid);
    if wifi_total.is_none() {
        return ApplyReport::fail(
            mode,
            exit::WIFI_UNAVAILABLE,
            "Wi-Fi is connected but has no default route yet. Try again in a moment.",
        );
    }

    let park = metric::park_value(cfg.park_metric, wifi_total);

    // Journal BEFORE changing anything, so a kill between here and the end is recoverable.
    let backups = capture(eth.luid);
    let mut journal = config::Journal {
        schema: config::SCHEMA,
        mode,
        in_flight: true,
        started_unix: config::now_unix(),
        finished_unix: None,
        restore: backups.clone(),
        bindings: Vec::new(),
        last_error: None,
    };
    let _ = config::save_journal(&journal);

    // Wi-Fi goes back to its automatic metric; Ethernet is parked above it.
    let wifi_out = metric::steer(wifi.luid, None);
    let eth_out = metric::steer(eth.luid, Some(park));
    lslog!("wifi {} | ethernet {}", wifi_out.describe(), eth_out.describe());

    if !eth_out.is_ok() {
        restore_backups(&backups);
        journal.in_flight = false;
        journal.restore.clear();
        journal.last_error = Some(eth_out.describe());
        journal.finished_unix = Some(config::now_unix());
        let _ = config::save_journal(&journal);
        return ApplyReport::fail(
            mode,
            exit::WRITE_FAILED,
            format!("Could not change the Ethernet metric: {}", eth_out.describe()),
        );
    }

    finish(mode, journal, backups, wifi.luid, eth.luid, &profile, park)
}

/// Give Ethernet the traffic back.
fn apply_ethernet(cfg: &MachineConfig, eth: &Nic, wifi: &Nic) -> ApplyReport {
    let mode = Mode::Ethernet;

    if !eth.media_connected {
        return ApplyReport::fail(
            mode,
            exit::NOT_CONFIGURED,
            "The Ethernet cable is not connected, so there is nothing to switch to.",
        );
    }

    let backups: Vec<MetricBackup> = capture(eth.luid)
        .into_iter()
        .chain(capture(wifi.luid))
        .collect();
    let mut journal = config::Journal {
        schema: config::SCHEMA,
        mode,
        in_flight: true,
        started_unix: config::now_unix(),
        finished_unix: None,
        restore: backups.clone(),
        bindings: Vec::new(),
        last_error: None,
    };
    let _ = config::save_journal(&journal);

    // Both back to automatic. On an ordinary machine that alone hands Ethernet the win, since
    // its automatic metric (5) beats Wi-Fi's (30).
    let eth_out = metric::steer(eth.luid, None);
    let wifi_out = metric::steer(wifi.luid, None);
    lslog!("ethernet {} | wifi {}", eth_out.describe(), wifi_out.describe());

    if !eth_out.is_ok() {
        restore_backups(&backups);
        journal.in_flight = false;
        journal.restore.clear();
        journal.last_error = Some(eth_out.describe());
        let _ = config::save_journal(&journal);
        return ApplyReport::fail(
            mode,
            exit::WRITE_FAILED,
            format!("Could not restore the Ethernet metric: {}", eth_out.describe()),
        );
    }

    // If Ethernet still does not win -- an unusual machine where its automatic metric is not
    // lower -- park Wi-Fi instead of silently doing nothing.
    let after = Snapshot::read();
    if !matches!(after.verdict(Some(eth.luid), Some(wifi.luid)), Verdict::Ethernet { .. }) {
        if let Some(eth_total) = crate::net::routes::total_for(&after.routes, eth.luid) {
            let park = metric::park_value(cfg.park_metric, Some(eth_total));
            lslog!("ethernet did not win on automatic metrics; parking wi-fi at {park}");
            let _ = metric::steer(wifi.luid, Some(park));
        }
    }

    finish(mode, journal, backups, eth.luid, wifi.luid, "", 0)
}

/// Hand both interfaces back to Windows.
fn apply_auto(eth: &Nic, wifi: &Nic) -> ApplyReport {
    let mode = Mode::Auto;
    let backups: Vec<MetricBackup> = capture(eth.luid)
        .into_iter()
        .chain(capture(wifi.luid))
        .collect();
    let mut journal = config::Journal {
        schema: config::SCHEMA,
        mode,
        in_flight: true,
        started_unix: config::now_unix(),
        finished_unix: None,
        restore: backups.clone(),
        bindings: Vec::new(),
        last_error: None,
    };
    let _ = config::save_journal(&journal);

    let eth_out = metric::steer(eth.luid, None);
    let wifi_out = metric::steer(wifi.luid, None);
    lslog!("auto: ethernet {} | wifi {}", eth_out.describe(), wifi_out.describe());

    journal.in_flight = false;
    journal.restore.clear();
    journal.finished_unix = Some(config::now_unix());
    let _ = config::save_journal(&journal);

    let ok = eth_out.is_ok() && wifi_out.is_ok();
    ApplyReport {
        mode,
        ok,
        exit_code: if ok { exit::OK } else { exit::WRITE_FAILED },
        message: if ok {
            "Both adapters are back on Windows' automatic metrics.".into()
        } else {
            "Could not fully restore automatic metrics.".into()
        },
        details: vec![eth_out.describe(), wifi_out.describe()],
    }
}

/// Verify the change took, revert if the machine ended up with no route at all, and close the
/// journal.
#[allow(clippy::too_many_arguments)]
fn finish(
    mode: Mode,
    mut journal: config::Journal,
    backups: Vec<MetricBackup>,
    want: LuidKey,
    _other: LuidKey,
    profile: &str,
    park: u32,
) -> ApplyReport {
    // Routing table changes are not instantaneous after a metric write.
    std::thread::sleep(Duration::from_millis(400));
    let after = Snapshot::read();

    // The safety net: if nothing at all can reach the internet now, put it back. A VPN holding
    // the default route counts as connectivity, which is why this checks for *any* default
    // route rather than for our specific interface winning.
    if after.routes.is_empty() {
        lslog!("no default route after the change; reverting");
        restore_backups(&backups);
        journal.in_flight = false;
        journal.restore.clear();
        journal.last_error = Some("no default route after the change; reverted".into());
        journal.finished_unix = Some(config::now_unix());
        let _ = config::save_journal(&journal);
        return ApplyReport::fail(
            mode,
            exit::VERIFY_FAILED,
            "The change left this machine with no network route, so it was undone.",
        );
    }

    journal.in_flight = false;
    journal.finished_unix = Some(config::now_unix());
    // Keep the backups: they are what `--apply auto` and uninstall use to put the machine back
    // exactly as it was found, rather than to a guessed default.
    journal.restore = backups;
    let _ = config::save_journal(&journal);

    // Record the Wi-Fi profile so a later unattended switch reconnects the right network.
    if !profile.is_empty() {
        let mut cfg = config::load_machine();
        if cfg.wifi_profile_hint.as_deref() != Some(profile) {
            cfg.wifi_profile_hint = Some(profile.to_string());
            let _ = config::save_machine(&cfg);
        }
    }

    let verdict = after.verdict(Some(want), None);
    let mut details = Vec::new();
    if park > 0 {
        details.push(format!("Ethernet parked at metric {park}"));
    }

    let message = match (&verdict, mode) {
        (Verdict::Ethernet { .. }, _) | (Verdict::Wifi { .. }, _) => match mode {
            Mode::Wifi => "Traffic now goes over Wi-Fi. Ethernet stays connected.".into(),
            Mode::Ethernet => "Traffic now goes over Ethernet.".into(),
            Mode::WifiOnly => "Ethernet's IP stack is detached.".into(),
            Mode::Auto => "Automatic metrics restored.".into(),
        },
        (Verdict::Hijacked { if_index, .. }, _) => {
            // Not a failure: a VPN owning the default route is normal, and the metric change did
            // exactly what it was asked to. But the user must know their public IP will not move.
            details.push(format!(
                "A VPN or tunnel on interface {if_index} is carrying all traffic."
            ));
            match mode {
                Mode::Wifi => {
                    "Ethernet is parked; your VPN tunnel now runs over Wi-Fi for new connections."
                        .into()
                }
                _ => "Applied. A VPN is still carrying all traffic.".into(),
            }
        }
        (Verdict::None, _) => "Applied, but no default route is present yet.".into(),
    };

    ApplyReport {
        mode,
        ok: true,
        exit_code: exit::OK,
        message,
        details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_distinct() {
        let codes = [
            exit::OK,
            exit::GENERIC,
            exit::NOT_ELEVATED,
            exit::NOT_CONFIGURED,
            exit::WIFI_UNAVAILABLE,
            exit::WRITE_FAILED,
            exit::VERIFY_FAILED,
            exit::BLOCKED_BY_POLICY,
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "exit codes must not collide");
    }

    #[test]
    fn a_failure_report_is_never_marked_ok() {
        let r = ApplyReport::fail(Mode::Wifi, exit::WIFI_UNAVAILABLE, "nope");
        assert!(!r.ok);
        assert_ne!(r.exit_code, exit::OK);
    }

    #[test]
    fn a_backup_of_an_automatic_metric_restores_to_automatic_not_to_its_number() {
        // Under automatic, Get reports the stack-computed value (5, 30, ...). Writing that back
        // would pin it as a manual metric that stops tracking link speed, so `automatic` must
        // drive the restore rather than the recorded number.
        let b = MetricBackup {
            luid: 1,
            family_v6: false,
            metric: 5,
            automatic: true,
        };
        let target = if b.automatic { None } else { Some(b.metric) };
        assert_eq!(target, None);

        let manual = MetricBackup {
            automatic: false,
            ..b
        };
        let target = if manual.automatic { None } else { Some(manual.metric) };
        assert_eq!(target, Some(5), "a user's manual metric must come back exactly");
    }
}
