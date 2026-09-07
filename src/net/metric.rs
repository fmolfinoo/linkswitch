//! Reading, pinning and restoring an interface's routing metric.
//!
//! This is the mechanism the whole app is built on. Windows picks an outbound interface by
//! longest-prefix match first and then by the *lowest total metric*, where total metric is
//! `route metric + interface metric`. DHCP-installed default routes carry a route metric of 0,
//! so in practice the interface metric alone decides which link carries your traffic.
//!
//! Raising Ethernet's interface metric above Wi-Fi's therefore moves new connections onto Wi-Fi
//! while Ethernet stays enabled, link-up, addressed and reachable on its own subnet. Nothing is
//! disabled and no cable is touched.
//!
//! Every trap documented below was hit for real; none of them are hypothetical.

use windows::Win32::Foundation::{NO_ERROR, WIN32_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIpInterfaceEntry, GetIpInterfaceTable, SetIpInterfaceEntry,
    MIB_IPINTERFACE_ROW, MIB_IPINTERFACE_TABLE,
};
use windows::Win32::Networking::WinSock::{ADDRESS_FAMILY, AF_INET, AF_INET6, AF_UNSPEC};

use super::err;
use super::luid::LuidKey;

/// What a single address family's write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyOutcome {
    /// The metric was written (or already held the requested value).
    Applied {
        before: u32,
        before_auto: bool,
        after: u32,
        after_auto: bool,
    },
    /// `ERROR_NOT_FOUND`: this address family is not bound to this adapter.
    ///
    /// This is **success**, not failure. A machine with IPv6 leak protection from a VPN has
    /// `ms_tcpip6` unbound on every physical NIC, so the IPv6 leg legitimately has nothing to do.
    /// Treating it as an error here would abort the flip and discard a perfectly good IPv4 write.
    FamilyAbsent,
    /// The adapter itself is gone (unplugged, disabled, driver replaced).
    AdapterGone,
    Failed(WIN32_ERROR),
}

impl FamilyOutcome {
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Applied { .. } | Self::FamilyAbsent)
    }

    /// True only when we actually wrote something.
    pub fn changed(self) -> bool {
        match self {
            Self::Applied {
                before,
                before_auto,
                after,
                after_auto,
            } => before != after || before_auto != after_auto,
            _ => false,
        }
    }

    pub fn describe(self) -> String {
        match self {
            Self::Applied {
                before,
                before_auto,
                after,
                after_auto,
            } => format!(
                "{} -> {}",
                fmt_metric(before, before_auto),
                fmt_metric(after, after_auto)
            ),
            Self::FamilyAbsent => "not bound on this adapter".into(),
            Self::AdapterGone => "adapter no longer present".into(),
            Self::Failed(rc) => err::describe(rc),
        }
    }
}

fn fmt_metric(m: u32, auto: bool) -> String {
    if auto {
        format!("automatic ({m})")
    } else {
        format!("manual {m}")
    }
}

/// A metric reading for one (interface, family) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricState {
    pub metric: u32,
    pub automatic: bool,
    /// A VPN or similar has told the stack to ignore this interface's default routes. When this
    /// is set, changing the metric has no effect on where traffic goes, and the UI must say so
    /// rather than let the user believe LinkSwitch is broken.
    pub default_routes_disabled: bool,
}

/// Populate a fresh row addressed by LUID + family.
///
/// TRAP 1: build a **fresh zeroed row per (luid, family)**. `MIB_IPINTERFACE_ROW` is `Copy`, and
/// reusing one variable across the v4 then v6 pass silently carries a stale `Family` into the
/// second call.
///
/// TRAP 2: do **not** call `InitializeIpInterfaceEntry`. It fills the struct with `0xFF` sentinel
/// bytes, including the offsets windows-rs types as Rust `bool`. A `bool` holding `0xFF` is an
/// invalid bit pattern, so merely *reading* `UseAutomaticMetric` afterwards is undefined
/// behaviour. `::default()` is a plain zeroed struct and is the correct thing here.
fn row_for(luid: LuidKey, family: ADDRESS_FAMILY) -> MIB_IPINTERFACE_ROW {
    let mut row = MIB_IPINTERFACE_ROW::default();
    // AF_UNSPEC is rejected with ERROR_INVALID_PARAMETER for the single-entry Get/Set calls.
    row.Family = family;
    row.InterfaceLuid = luid.into();
    // InterfaceIndex deliberately left 0: a nonzero LUID takes precedence, and the index is not
    // persistent across reboots.
    row
}

/// Read the current metric for one (interface, family) pair. Does not require elevation.
pub fn read(luid: LuidKey, family: ADDRESS_FAMILY) -> Result<MetricState, WIN32_ERROR> {
    let mut row = row_for(luid, family);
    // SAFETY: `row` is a valid, fully-initialised struct addressed by LUID + Family, which is
    // what the API requires. The call only reads through the pointer and writes back into it.
    let rc = unsafe { GetIpInterfaceEntry(&mut row) };
    if rc != NO_ERROR {
        return Err(rc);
    }
    Ok(MetricState {
        metric: row.Metric,
        automatic: row.UseAutomaticMetric,
        default_routes_disabled: row.DisableDefaultRoutes,
    })
}

/// Pin a manual metric (`Some`) or hand the interface back to Windows' automatic metric (`None`).
///
/// The caller must be elevated. Unelevated, `SetIpInterfaceEntry` returns `ERROR_ACCESS_DENIED`,
/// and merely being a member of Administrators is not enough under a UAC filtered token.
pub fn set(luid: LuidKey, family: ADDRESS_FAMILY, target: Option<u32>) -> FamilyOutcome {
    let mut row = row_for(luid, family);

    // SAFETY: as in `read`.
    let rc = unsafe { GetIpInterfaceEntry(&mut row) };
    if rc != NO_ERROR {
        if err::is_family_absent(rc) {
            return FamilyOutcome::FamilyAbsent;
        }
        if err::is_adapter_gone(rc) {
            return FamilyOutcome::AdapterGone;
        }
        return FamilyOutcome::Failed(rc);
    }

    let before = row.Metric;
    let before_auto = row.UseAutomaticMetric;

    match target {
        Some(m) => {
            // TRAP 3: `Metric` is ignored entirely unless `UseAutomaticMetric` is false.
            // TRAP 4: both are plain Rust `bool` in windows-rs, not `u8` and not `BOOL`.
            row.UseAutomaticMetric = false;
            row.Metric = m;
        }
        None => {
            // TRAP 5: automatic cannot be restored by writing back a remembered number. Under
            // automatic, Get reports the *stack-computed* value (5, 25, 30, 35, ...); writing
            // that back pins it as a manual metric that no longer tracks link speed.
            row.UseAutomaticMetric = true;
            row.Metric = 0;
        }
    }

    // TRAP 6, the one that actually blocks the write. Microsoft's SetIpInterfaceEntry remarks
    // state it twice: for IPv4 an application must not modify SitePrefixLength, and it must be
    // set to 0. Get hands back 64 for AF_INET on ordinary auto-metric rows, while the docs also
    // say anything above 32 is illegal for IPv4 -- pass it straight through and the write fails
    // with ERROR_INVALID_PARAMETER. For IPv6 round-trip whatever Get returned.
    if family == AF_INET {
        row.SitePrefixLength = 0;
    }

    // SAFETY: `row` was populated by a successful Get and then modified only in fields the API
    // documents as writable.
    let rc = unsafe { SetIpInterfaceEntry(&mut row) };
    if rc == NO_ERROR {
        FamilyOutcome::Applied {
            before,
            before_auto,
            after: target.unwrap_or(0),
            after_auto: target.is_none(),
        }
    } else if err::is_adapter_gone(rc) {
        FamilyOutcome::AdapterGone
    } else {
        FamilyOutcome::Failed(rc)
    }
}

/// The result of steering one interface across both address families.
#[derive(Debug, Clone, Copy)]
pub struct SteerOutcome {
    pub v4: FamilyOutcome,
    pub v6: FamilyOutcome,
}

impl SteerOutcome {
    /// Both families must be OK.
    ///
    /// IPv6 being *absent* counts as OK, but IPv6 being present and *failing* does not. Windows
    /// prefers IPv6 over IPv4 under RFC 6724, so a silently-failed v6 leg would leave traffic
    /// egressing the old adapter while the UI cheerfully reported success.
    pub fn is_ok(&self) -> bool {
        self.v4.is_ok() && self.v6.is_ok()
    }

    pub fn changed(&self) -> bool {
        self.v4.changed() || self.v6.changed()
    }

    pub fn describe(&self) -> String {
        format!("IPv4: {} | IPv6: {}", self.v4.describe(), self.v6.describe())
    }
}

/// Steer both address families of one interface.
pub fn steer(luid: LuidKey, target: Option<u32>) -> SteerOutcome {
    SteerOutcome {
        v4: set(luid, AF_INET, target),
        v6: set(luid, AF_INET6, target),
    }
}

/// One row of the bulk interface-metric snapshot.
#[derive(Debug, Clone, Copy)]
pub struct IfaceMetric {
    pub luid: LuidKey,
    pub if_index: u32,
    pub family: ADDRESS_FAMILY,
    pub metric: u32,
    pub automatic: bool,
    pub connected: bool,
    pub default_routes_disabled: bool,
}

/// Read every interface's metric in one unprivileged call. Used by the widget each refresh.
pub fn snapshot() -> Vec<IfaceMetric> {
    let mut out = Vec::new();
    let mut table: *mut MIB_IPINTERFACE_TABLE = std::ptr::null_mut();

    // AF_UNSPEC is legal for the *table* call (unlike the single-entry Get/Set).
    // SAFETY: `table` is a valid out-pointer; on success the OS allocates and we free below.
    let rc = unsafe { GetIpInterfaceTable(AF_UNSPEC, &mut table) };
    if rc != NO_ERROR || table.is_null() {
        return out;
    }

    // SAFETY: the table is a C ANYSIZE_ARRAY: `Table` is declared `[MIB_IPINTERFACE_ROW; 1]` but
    // actually holds `NumEntries` rows. Indexing the Rust array beyond 0 would panic on a bounds
    // check, so build a slice from the raw pointer instead.
    unsafe {
        let n = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        out.reserve(n);
        for r in rows {
            out.push(IfaceMetric {
                luid: LuidKey::from(r.InterfaceLuid),
                if_index: r.InterfaceIndex,
                family: r.Family,
                metric: r.Metric,
                automatic: r.UseAutomaticMetric,
                connected: r.Connected,
                default_routes_disabled: r.DisableDefaultRoutes,
            });
        }
        FreeMibTable(table as *const core::ffi::c_void);
    }
    out
}

/// Look up one interface's metric in a snapshot.
pub fn find(snap: &[IfaceMetric], luid: LuidKey, family: ADDRESS_FAMILY) -> Option<IfaceMetric> {
    snap.iter()
        .find(|m| m.luid == luid && m.family == family)
        .copied()
}

/// Metric values LinkSwitch is willing to write.
///
/// The worker validates against this range no matter what the config file says, so a tampered
/// config cannot push an interface into a nonsensical state.
pub const PARK_MIN: u32 = 100;
pub const PARK_MAX: u32 = 9999;

/// Default "park" metric: high enough to lose to any automatic metric Windows assigns.
///
/// Windows' automatic metrics top out at 75 for real interfaces (loopback), and the highest
/// value seen on ordinary hardware is 65. 9000 clears every one of them with room to spare while
/// staying inside the documented range.
pub const PARK_DEFAULT: u32 = 9000;

/// Choose a park metric guaranteed to lose against `rival_total`.
///
/// Hardcoding 9000 is *usually* right, but a persistent route added with an explicit metric
/// offset (`route -p add ... metric 256`) can lift the rival's total above a naive guess. This
/// computes a value that actually beats what is on the machine right now.
pub fn park_value(configured: u32, rival_total: Option<u32>) -> u32 {
    let base = configured.clamp(PARK_MIN, PARK_MAX);
    match rival_total {
        // Stay at least 100 above the rival so a small automatic-metric drift cannot flip it back.
        Some(rival) if base <= rival.saturating_add(100) => {
            rival.saturating_add(100).clamp(PARK_MIN, PARK_MAX)
        }
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn park_value_clamps_into_range() {
        assert_eq!(park_value(0, None), PARK_MIN);
        assert_eq!(park_value(50_000, None), PARK_MAX);
        assert_eq!(park_value(9000, None), 9000);
    }

    #[test]
    fn park_value_beats_a_high_rival() {
        // A rival total of 9000 must not be "parked" against with 9000 -- ties are unspecified.
        assert!(park_value(9000, Some(9000)) > 9000 - 1);
        assert_eq!(park_value(9000, Some(9000)), 9100.min(PARK_MAX));
        assert_eq!(park_value(500, Some(5000)), 5100);
    }

    #[test]
    fn park_value_leaves_a_comfortable_default_alone() {
        // Ordinary case: Wi-Fi at total 30, configured park 9000 -- no adjustment needed.
        assert_eq!(park_value(9000, Some(30)), 9000);
    }

    #[test]
    fn family_absent_is_success_but_not_a_change() {
        assert!(FamilyOutcome::FamilyAbsent.is_ok());
        assert!(!FamilyOutcome::FamilyAbsent.changed());
    }

    #[test]
    fn a_present_but_failed_family_is_not_success() {
        use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
        assert!(!FamilyOutcome::Failed(ERROR_ACCESS_DENIED).is_ok());
        assert!(!FamilyOutcome::AdapterGone.is_ok());
    }

    #[test]
    fn steer_outcome_requires_both_legs() {
        use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
        let applied = FamilyOutcome::Applied {
            before: 5,
            before_auto: true,
            after: 9000,
            after_auto: false,
        };
        assert!(SteerOutcome {
            v4: applied,
            v6: FamilyOutcome::FamilyAbsent
        }
        .is_ok());
        // v6 bound and failing must sink the whole flip: Windows prefers IPv6.
        assert!(!SteerOutcome {
            v4: applied,
            v6: FamilyOutcome::Failed(ERROR_ACCESS_DENIED)
        }
        .is_ok());
    }

    #[test]
    fn applied_detects_no_op_writes() {
        let same = FamilyOutcome::Applied {
            before: 9000,
            before_auto: false,
            after: 9000,
            after_auto: false,
        };
        assert!(same.is_ok());
        assert!(!same.changed());
    }
}
