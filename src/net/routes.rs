//! Which interface is actually carrying traffic right now.
//!
//! Deliberately **not** built on a bare `GetBestRoute2`. On a machine with a VPN, an unpinned
//! `GetBestRoute2(8.8.8.8)` answers with the tunnel interface -- neither of our candidates -- and
//! the documented contract requires at least one of `InterfaceLuid`/`InterfaceIndex` to be
//! initialised anyway. Instead we read the forwarding table, keep the default routes, and add
//! each one's interface metric to get the total Windows itself compares.

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIpForwardTable2, MIB_IPFORWARD_TABLE2,
};
use windows::Win32::Networking::WinSock::{ADDRESS_FAMILY, AF_INET, AF_INET6};

use super::luid::LuidKey;
use super::metric::IfaceMetric;

/// A default route (`0.0.0.0/0` or `::/0`) and its true cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultRoute {
    pub luid: LuidKey,
    pub if_index: u32,
    /// `MIB_IPFORWARD_ROW2::Metric` -- the route's own offset.
    pub route_metric: u32,
    /// The interface metric for this (luid, family).
    pub iface_metric: u32,
    /// What Windows compares: `route_metric + iface_metric`.
    pub total: u32,
    pub family_is_v6: bool,
}

/// Who is winning the race for internet traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Ethernet {
        total: u32,
    },
    Wifi {
        total: u32,
    },
    /// Some third interface -- a VPN tunnel, a virtual switch -- holds the best default route.
    ///
    /// This is a normal state, not an error. Surfacing it is what stops a VPN user concluding
    /// that LinkSwitch does nothing: their public IP will not change whichever link is chosen,
    /// because the tunnel is what actually reaches the internet.
    Hijacked {
        luid: LuidKey,
        if_index: u32,
        total: u32,
    },
    /// Neither candidate has a default route: cable out and Wi-Fi down.
    None,
}

/// Read every default route for one address family.
pub fn default_routes(family: ADDRESS_FAMILY, metrics: &[IfaceMetric]) -> Vec<DefaultRoute> {
    let mut out = Vec::new();
    let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();

    // SAFETY: `table` is a valid out-pointer; on success the OS allocates and we free below.
    let rc = unsafe { GetIpForwardTable2(family, &mut table) };
    if rc != NO_ERROR || table.is_null() {
        return out;
    }

    // SAFETY: C ANYSIZE_ARRAY -- `Table` is declared as a 1-element array but holds `NumEntries`
    // rows, so it must be read through a raw slice rather than by indexing the Rust array.
    unsafe {
        let n = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        for r in rows {
            if r.DestinationPrefix.PrefixLength != 0 {
                continue;
            }
            let luid = LuidKey::from(r.InterfaceLuid);
            let iface_metric = metrics
                .iter()
                .find(|m| m.luid == luid && m.family == family)
                .map(|m| m.metric)
                .unwrap_or(0);
            out.push(DefaultRoute {
                luid,
                if_index: r.InterfaceIndex,
                route_metric: r.Metric,
                iface_metric,
                total: r.Metric.saturating_add(iface_metric),
                family_is_v6: family == AF_INET6,
            });
        }
        FreeMibTable(table as *const core::ffi::c_void);
    }
    out
}

/// Default routes for both families.
pub fn all_default_routes(metrics: &[IfaceMetric]) -> Vec<DefaultRoute> {
    let mut v = default_routes(AF_INET, metrics);
    v.extend(default_routes(AF_INET6, metrics));
    v
}

/// Decide who is winning, given the default routes and the two candidate interfaces.
///
/// Pure so it can be unit-tested without touching the machine.
pub fn winner(routes: &[DefaultRoute], eth: Option<LuidKey>, wifi: Option<LuidKey>) -> Verdict {
    // Only IPv4 participates in the verdict when IPv6 has no default route at all, which is the
    // common case on a machine with VPN leak protection. When both exist, the lowest total wins;
    // ties break toward IPv4 purely for display stability.
    let best = routes.iter().min_by_key(|r| (r.total, r.family_is_v6));
    let Some(best) = best else {
        return Verdict::None;
    };

    if Some(best.luid) == eth {
        return Verdict::Ethernet { total: best.total };
    }
    if Some(best.luid) == wifi {
        return Verdict::Wifi { total: best.total };
    }
    Verdict::Hijacked {
        luid: best.luid,
        if_index: best.if_index,
        total: best.total,
    }
}

/// Best total metric among the routes belonging to one interface, if it has any.
pub fn total_for(routes: &[DefaultRoute], luid: LuidKey) -> Option<u32> {
    routes
        .iter()
        .filter(|r| r.luid == luid)
        .map(|r| r.total)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETH: LuidKey = LuidKey(0x1111);
    const WIFI: LuidKey = LuidKey(0x2222);
    const VPN: LuidKey = LuidKey(0x3333);

    fn route(luid: LuidKey, route_metric: u32, iface_metric: u32) -> DefaultRoute {
        DefaultRoute {
            luid,
            if_index: 1,
            route_metric,
            iface_metric,
            total: route_metric + iface_metric,
            family_is_v6: false,
        }
    }

    #[test]
    fn no_routes_means_no_winner() {
        assert_eq!(winner(&[], Some(ETH), Some(WIFI)), Verdict::None);
    }

    #[test]
    fn lowest_total_wins_not_lowest_interface_metric() {
        // Ethernet has the lower *interface* metric but a route offset pushes its total above
        // Wi-Fi's. Windows compares the sum, so Wi-Fi must win.
        let routes = [route(ETH, 500, 5), route(WIFI, 0, 30)];
        assert_eq!(winner(&routes, Some(ETH), Some(WIFI)), Verdict::Wifi { total: 30 });
    }

    #[test]
    fn parked_ethernet_hands_the_win_to_wifi() {
        let routes = [route(ETH, 0, 9000), route(WIFI, 0, 30)];
        assert_eq!(winner(&routes, Some(ETH), Some(WIFI)), Verdict::Wifi { total: 30 });
    }

    #[test]
    fn default_state_gives_ethernet_the_win() {
        // The machine's real automatic metrics: Ethernet 5, Wi-Fi 30.
        let routes = [route(ETH, 0, 5), route(WIFI, 0, 30)];
        assert_eq!(winner(&routes, Some(ETH), Some(WIFI)), Verdict::Ethernet { total: 5 });
    }

    #[test]
    fn a_vpn_holding_the_best_route_is_reported_as_hijacked() {
        let routes = [route(VPN, 0, 0), route(WIFI, 0, 30)];
        match winner(&routes, Some(ETH), Some(WIFI)) {
            Verdict::Hijacked { luid, total, .. } => {
                assert_eq!(luid, VPN);
                assert_eq!(total, 0);
            }
            other => panic!("expected Hijacked, got {other:?}"),
        }
    }

    #[test]
    fn an_unplugged_interface_simply_has_no_route() {
        // Cable out: Ethernet contributes no default route at all, so Wi-Fi wins by default.
        let routes = [route(WIFI, 0, 30)];
        assert_eq!(winner(&routes, Some(ETH), Some(WIFI)), Verdict::Wifi { total: 30 });
        assert_eq!(total_for(&routes, ETH), None);
        assert_eq!(total_for(&routes, WIFI), Some(30));
    }

    #[test]
    fn winner_is_stable_when_a_candidate_is_not_configured() {
        let routes = [route(WIFI, 0, 30)];
        // No Ethernet configured yet -- Wi-Fi still resolves correctly rather than reading as
        // hijacked.
        assert_eq!(winner(&routes, None, Some(WIFI)), Verdict::Wifi { total: 30 });
    }

    #[test]
    fn total_for_picks_the_cheapest_route_on_an_interface() {
        let routes = [route(WIFI, 0, 30), route(WIFI, 100, 30)];
        assert_eq!(total_for(&routes, WIFI), Some(30));
    }
}
