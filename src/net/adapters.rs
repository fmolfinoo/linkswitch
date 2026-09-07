//! Adapter enumeration and classification.
//!
//! The hard part is not listing adapters, it is telling a real NIC apart from the pile of
//! virtual ones a normal machine accumulates. On the development machine `GetAdaptersAddresses`
//! returns, among others: two VMware VMnet adapters, a Hyper-V Default Switch, a Bluetooth PAN,
//! two "Microsoft Wi-Fi Direct Virtual Adapter" pseudo-devices, a loopback and a WireGuard VPN
//! tunnel.
//!
//! `IfType` alone cannot do it. VMware VMnet and Hyper-V vEthernet adapters also report
//! `IF_TYPE_ETHERNET_CSMACD` (6), and the Wi-Fi Direct pseudo-adapters also report
//! `IF_TYPE_IEEE80211` (71). Description matching cannot do it either -- those strings are
//! localised. The reliable signal is `GetIfEntry2`'s `InterfaceAndOperStatusFlags` plus
//! `PhysicalMediumType`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GetIfEntry2, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
    GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2,
};
use windows::Win32::NetworkManagement::Ndis::{
    IfOperStatusUp, MediaConnectStateConnected, NdisPhysicalMedium802_3,
    NdisPhysicalMediumNative802_11, NdisPhysicalMediumWirelessLan, NdisPhysicalMediumWirelessWan,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6};

use super::luid::LuidKey;

pub const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
pub const IF_TYPE_IEEE80211: u32 = 71;

// MIB_IF_ROW2::InterfaceAndOperStatusFlags is collapsed by windows-rs into an opaque byte with
// no accessors, so the bit positions are spelled out here. Order per the Microsoft header.
const FLAG_HARDWARE_INTERFACE: u8 = 0x01;
const FLAG_FILTER_INTERFACE: u8 = 0x02;
const FLAG_CONNECTOR_PRESENT: u8 = 0x04;
const FLAG_NOT_AUTHENTICATED: u8 = 0x08;
const FLAG_NOT_MEDIA_CONNECTED: u8 = 0x10;
const FLAG_PAUSED: u8 = 0x20;
const FLAG_LOW_POWER: u8 = 0x40;
const FLAG_END_POINT_INTERFACE: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NicKind {
    Ethernet,
    Wifi,
    Other,
}

/// How confident we are that this adapter is a real, steerable link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Real hardware with a physical connector. What we want.
    Hardware,
    /// Not hardware, but it carries an IP address and looks like the interface that actually
    /// holds the stack -- the Hyper-V external-vSwitch and NIC-teaming case, where the physical
    /// NIC has no IP and a `vEthernet (...)` adapter carries everything.
    Virtualised,
    /// Everything else: tunnels, host-only switches, Wi-Fi Direct, loopback.
    Rejected,
}

#[derive(Debug, Clone)]
pub struct Nic {
    /// Stable identity. Persist this, never `if_index`.
    pub luid: LuidKey,
    pub if_index: u32,
    pub friendly_name: String,
    pub description: String,
    /// The adapter GUID string, e.g. `{66666666-...}`. Secondary identity for config recovery.
    pub adapter_name: String,
    pub if_type: u32,
    pub kind: NicKind,
    pub tier: Tier,
    pub oper_up: bool,
    /// `MediaConnectState == Connected`. For Ethernet this is literally "is the cable in?", and
    /// the widget must show it: "parked by LinkSwitch" and "cable unplugged" look identical if
    /// you only watch the routing table.
    pub media_connected: bool,
    pub tx_speed_bps: Option<u64>,
    /// Current MAC. Display only -- Wi-Fi "random hardware addresses" makes it drift.
    pub mac: Option<[u8; 6]>,
    /// Burned-in MAC from `MIB_IF_ROW2::PermanentPhysicalAddress`. This is the durable
    /// identity: it survives reboots, driver reinstalls, dock reconnects and MAC
    /// randomisation, none of which the LUID or the current MAC do.
    pub permanent_mac: Option<[u8; 6]>,
    pub ipv4: Vec<Ipv4Addr>,
    pub ipv6: Vec<Ipv6Addr>,
    pub gateways: Vec<IpAddr>,
}

impl Nic {
    /// A one-line label for the UI.
    pub fn label(&self) -> &str {
        if self.friendly_name.is_empty() {
            &self.description
        } else {
            &self.friendly_name
        }
    }

    pub fn link_speed_text(&self) -> Option<String> {
        let bps = self.tx_speed_bps?;
        Some(if bps >= 1_000_000_000 {
            format!("{:.1} Gbps", bps as f64 / 1e9)
        } else {
            format!("{} Mbps", bps / 1_000_000)
        })
    }

    /// Usable as a switch target right now.
    pub fn is_live(&self) -> bool {
        self.oper_up && self.media_connected && !self.ipv4.is_empty()
    }
}

/// Enumerate every adapter, classified.
pub fn enumerate() -> Vec<Nic> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;

    // Microsoft's own retry algorithm. Three tries, not one: adapters really can appear between
    // the sizing call and the data call (a VPN connecting, Wi-Fi Direct spinning up a pseudo
    // adapter), and a single retry is exactly the bug that only ever reproduces on someone
    // else's machine.
    const MAX_TRIES: u32 = 3;
    let mut size: u32 = 15_000;
    let mut tries = 0u32;

    loop {
        // Vec<u64> rather than Vec<u8>: IP_ADAPTER_ADDRESSES_LH needs 8-byte alignment and
        // Vec<u8> guarantees only 1. Casting an under-aligned buffer is undefined behaviour.
        let mut buf: Vec<u64> = vec![0u64; (size as usize).div_ceil(8) + 1];
        let head = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;

        // SAFETY: `head` points at a correctly aligned buffer of `size` bytes, and `size` is
        // updated by the call to the required length.
        let rc = unsafe {
            GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(head), &mut size)
        };

        tries += 1;
        if rc == NO_ERROR.0 {
            // SAFETY: the call succeeded, so the buffer holds a valid linked list. `buf` outlives
            // the walk.
            return unsafe { walk(head) };
        }
        if rc != windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW.0 || tries >= MAX_TRIES {
            return Vec::new();
        }
    }
}

/// SAFETY: `head` must point at a valid `IP_ADAPTER_ADDRESSES_LH` linked list that outlives
/// this call.
unsafe fn walk(head: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<Nic> {
    let mut out = Vec::new();
    let mut cur = head;

    while !cur.is_null() {
        let a = &*cur;
        cur = a.Next;

        let luid = LuidKey::from(a.Luid);
        if luid.is_zero() {
            continue;
        }

        // IfIndex lives inside a nested anonymous union.
        let if_index = a.Anonymous1.Anonymous.IfIndex;
        let if_type = a.IfType;

        let friendly_name = pwstr_to_string(a.FriendlyName.0);
        let description = pwstr_to_string(a.Description.0);
        let adapter_name = pstr_to_string(a.AdapterName.0);

        let mac = if a.PhysicalAddressLength == 6 {
            let mut m = [0u8; 6];
            m.copy_from_slice(&a.PhysicalAddress[..6]);
            Some(m)
        } else {
            None
        };

        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        let mut uni = a.FirstUnicastAddress;
        while !uni.is_null() {
            let sa = (*uni).Address.lpSockaddr;
            if !sa.is_null() {
                match sockaddr_to_ip(sa) {
                    Some(IpAddr::V4(v)) => ipv4.push(v),
                    Some(IpAddr::V6(v)) => ipv6.push(v),
                    None => {}
                }
            }
            uni = (*uni).Next;
        }

        let mut gateways = Vec::new();
        let mut gw = a.FirstGatewayAddress;
        while !gw.is_null() {
            let sa = (*gw).Address.lpSockaddr;
            if !sa.is_null() {
                if let Some(ip) = sockaddr_to_ip(sa) {
                    gateways.push(ip);
                }
            }
            gw = (*gw).Next;
        }

        let routable = Routable {
            usable_ip: ipv4.iter().any(is_usable_v4) || ipv6.iter().any(is_usable_v6),
            gateway: !gateways.is_empty(),
        };

        let detail = if_detail(luid);
        let (kind, tier, media_connected, tx_speed_bps, oper_up, permanent_mac) = match detail {
            Some(d) => (
                d.kind,
                classify_tier(&d, routable),
                d.media_connected,
                d.tx_speed_bps,
                d.oper_up,
                d.permanent_mac,
            ),
            None => (
                kind_from_if_type(if_type),
                Tier::Rejected,
                false,
                None,
                a.OperStatus == IfOperStatusUp,
                None,
            ),
        };

        out.push(Nic {
            luid,
            if_index,
            friendly_name,
            description,
            adapter_name,
            if_type,
            kind,
            tier,
            oper_up,
            media_connected,
            tx_speed_bps,
            mac,
            permanent_mac,
            ipv4,
            ipv6,
            gateways,
        });
    }
    out
}

struct IfDetail {
    kind: NicKind,
    hardware: bool,
    connector_present: bool,
    filter_interface: bool,
    end_point: bool,
    media_connected: bool,
    oper_up: bool,
    tx_speed_bps: Option<u64>,
    permanent_mac: Option<[u8; 6]>,
}

fn if_detail(luid: LuidKey) -> Option<IfDetail> {
    let mut row = MIB_IF_ROW2 {
        InterfaceLuid: luid.into(),
        ..Default::default()
    };
    // SAFETY: `row` is addressed by LUID, which is what GetIfEntry2 requires; the call only
    // writes back into the struct.
    if unsafe { GetIfEntry2(&mut row) } != NO_ERROR {
        return None;
    }

    let f = row.InterfaceAndOperStatusFlags._bitfield;
    let speed = row.TransmitLinkSpeed;

    // Physical medium is locale-independent and beats every string heuristic.
    // Note NdisPhysicalMediumWirelessLan == 1 is *wireless*, not Ethernet -- a classic mix-up.
    // Ethernet is NdisPhysicalMedium802_3 == 14.
    let kind = match row.PhysicalMediumType {
        NdisPhysicalMedium802_3 => NicKind::Ethernet,
        NdisPhysicalMediumNative802_11 | NdisPhysicalMediumWirelessLan
        | NdisPhysicalMediumWirelessWan => NicKind::Wifi,
        _ => kind_from_if_type(row.Type),
    };

    let permanent_mac = (row.PhysicalAddressLength == 6).then(|| {
        let mut m = [0u8; 6];
        m.copy_from_slice(&row.PermanentPhysicalAddress[..6]);
        m
    });

    Some(IfDetail {
        kind,
        permanent_mac,
        hardware: f & FLAG_HARDWARE_INTERFACE != 0,
        connector_present: f & FLAG_CONNECTOR_PRESENT != 0,
        filter_interface: f & FLAG_FILTER_INTERFACE != 0,
        end_point: f & FLAG_END_POINT_INTERFACE != 0,
        media_connected: row.MediaConnectState == MediaConnectStateConnected
            && f & FLAG_NOT_MEDIA_CONNECTED == 0,
        oper_up: row.OperStatus == IfOperStatusUp,
        tx_speed_bps: (speed != u64::MAX && speed != 0).then_some(speed),
    })
}

/// Whether an adapter looks like it can actually carry traffic off this machine.
#[derive(Debug, Clone, Copy)]
struct Routable {
    /// Has at least one address that is not APIPA/link-local and not loopback.
    usable_ip: bool,
    /// Has at least one default gateway.
    gateway: bool,
}

/// An address Windows handed out because DHCP failed. Its presence means the opposite of
/// "this adapter works": every disconnected NIC on the machine has one.
fn is_usable_v4(a: &Ipv4Addr) -> bool {
    !a.is_link_local() && !a.is_loopback() && !a.is_unspecified()
}

fn is_usable_v6(a: &Ipv6Addr) -> bool {
    // `Ipv6Addr::is_unicast_link_local` is still unstable, so test the fe80::/10 prefix directly.
    let seg = a.segments()[0];
    (seg & 0xffc0) != 0xfe80 && !a.is_loopback() && !a.is_unspecified()
}

fn classify_tier(d: &IfDetail, r: Routable) -> Tier {
    if d.kind == NicKind::Other {
        return Tier::Rejected;
    }
    // An end-point interface is by definition not a real network link.
    if d.end_point {
        return Tier::Rejected;
    }
    if d.hardware && d.connector_present && !d.filter_interface {
        return Tier::Hardware;
    }
    // Hyper-V external vSwitch / NIC teaming: the physical NIC is stripped of its IP stack and a
    // synthetic adapter carries the address and the default route, so rejecting every
    // non-hardware interface would reject exactly the one that has to be steered.
    //
    // Both conditions are required. Demanding only an IP address lets in every virtual adapter
    // on an ordinary machine -- Bluetooth PAN, the Hyper-V *internal* Default Switch, Wi-Fi
    // Direct pseudo-adapters and ICS hosts all carry an address. Requiring a default gateway as
    // well is what separates "this interface reaches the internet" from "this interface exists".
    if r.usable_ip && r.gateway && !d.filter_interface {
        return Tier::Virtualised;
    }
    Tier::Rejected
}

fn kind_from_if_type(if_type: u32) -> NicKind {
    match if_type {
        IF_TYPE_ETHERNET_CSMACD => NicKind::Ethernet,
        IF_TYPE_IEEE80211 => NicKind::Wifi,
        _ => NicKind::Other,
    }
}

/// Steerable candidates, split by kind and ordered best-first.
///
/// Ordering rule: prefer a live link (up, media connected, addressed), then a hardware tier over
/// a virtualised one, then a present gateway, then the faster link. That makes the zero-config
/// default -- take the first of each -- correct on an ordinary machine, while a docking station
/// with two Ethernet NICs still gets a stable, explainable pick that the user can override.
pub fn candidates() -> (Vec<Nic>, Vec<Nic>) {
    let all = enumerate();
    let mut eth: Vec<Nic> = all
        .iter()
        .filter(|n| n.kind == NicKind::Ethernet && n.tier != Tier::Rejected)
        .cloned()
        .collect();
    let mut wifi: Vec<Nic> = all
        .iter()
        .filter(|n| n.kind == NicKind::Wifi && n.tier != Tier::Rejected)
        .cloned()
        .collect();
    eth.sort_by(|a, b| rank(a).cmp(&rank(b)));
    wifi.sort_by(|a, b| rank(a).cmp(&rank(b)));
    (eth, wifi)
}

/// Sort key: smaller is better. Public so there is exactly one ranking rule in the codebase --
/// an unranked second copy silently put the real Wi-Fi adapter behind two Wi-Fi Direct
/// pseudo-adapters.
pub fn rank(n: &Nic) -> (u8, u8, u8, std::cmp::Reverse<u64>) {
    (
        u8::from(!n.is_live()),
        u8::from(n.tier != Tier::Hardware),
        u8::from(n.gateways.is_empty()),
        std::cmp::Reverse(n.tx_speed_bps.unwrap_or(0)),
    )
}

/// SAFETY: `p` must be null or a valid NUL-terminated UTF-16 string.
unsafe fn pwstr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
        if len > 4096 {
            break;
        }
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

/// SAFETY: `p` must be null or a valid NUL-terminated ANSI string.
unsafe fn pstr_to_string(p: *const u8) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
        if len > 4096 {
            break;
        }
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned()
}

/// SAFETY: `sa` must point at a valid `SOCKADDR` whose `sa_family` describes the storage.
unsafe fn sockaddr_to_ip(sa: *const windows::Win32::Networking::WinSock::SOCKADDR) -> Option<IpAddr> {
    match (*sa).sa_family {
        f if f == AF_INET => {
            let v4 = &*(sa as *const SOCKADDR_IN);
            Some(IpAddr::V4(Ipv4Addr::from(v4.sin_addr.S_un.S_addr.to_ne_bytes())))
        }
        f if f == AF_INET6 => {
            let v6 = &*(sa as *const SOCKADDR_IN6);
            Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.u.Byte)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(hardware: bool, connector: bool, filter: bool, end_point: bool) -> IfDetail {
        IfDetail {
            kind: NicKind::Ethernet,
            hardware,
            connector_present: connector,
            filter_interface: filter,
            end_point,
            media_connected: true,
            oper_up: true,
            tx_speed_bps: Some(1_000_000_000),
            permanent_mac: None,
        }
    }

    const ROUTABLE: Routable = Routable {
        usable_ip: true,
        gateway: true,
    };
    /// The shape every idle virtual adapter on a real machine has: an APIPA address and no
    /// gateway.
    const DEAD_END: Routable = Routable {
        usable_ip: false,
        gateway: false,
    };

    #[test]
    fn real_nic_is_hardware_tier() {
        assert_eq!(
            classify_tier(&detail(true, true, false, false), ROUTABLE),
            Tier::Hardware
        );
    }

    #[test]
    fn a_hardware_nic_stays_hardware_even_with_no_address() {
        // Cable unplugged: no usable IP, no gateway. It is still the adapter to steer.
        assert_eq!(
            classify_tier(&detail(true, true, false, false), DEAD_END),
            Tier::Hardware
        );
    }

    #[test]
    fn external_vswitch_carrying_the_stack_is_kept_as_a_fallback() {
        // Hyper-V external vSwitch: not hardware, but it holds the address and the gateway.
        assert_eq!(
            classify_tier(&detail(false, false, false, false), ROUTABLE),
            Tier::Virtualised
        );
    }

    #[test]
    fn internal_switches_and_pseudo_adapters_are_rejected() {
        // The real regression: Bluetooth PAN, the Hyper-V *internal* Default Switch, Wi-Fi
        // Direct "Local Area Connection* N" and ICS hosts all have an address but no gateway,
        // and were being offered as switch targets.
        let ip_but_no_gateway = Routable {
            usable_ip: true,
            gateway: false,
        };
        assert_eq!(
            classify_tier(&detail(false, false, false, false), ip_but_no_gateway),
            Tier::Rejected
        );
        // APIPA only, which is what a disconnected pseudo-adapter reports.
        assert_eq!(
            classify_tier(&detail(false, false, false, false), DEAD_END),
            Tier::Rejected
        );
    }

    #[test]
    fn filter_and_endpoint_interfaces_are_always_rejected() {
        assert_eq!(
            classify_tier(&detail(true, true, true, false), ROUTABLE),
            Tier::Rejected
        );
        assert_eq!(
            classify_tier(&detail(true, true, false, true), ROUTABLE),
            Tier::Rejected
        );
    }

    #[test]
    fn other_media_is_rejected_even_when_it_is_hardware() {
        let mut d = detail(true, true, false, false);
        d.kind = NicKind::Other;
        assert_eq!(classify_tier(&d, ROUTABLE), Tier::Rejected);
    }

    #[test]
    fn apipa_addresses_are_not_usable() {
        assert!(!is_usable_v4(&Ipv4Addr::new(169, 254, 166, 142)));
        assert!(!is_usable_v4(&Ipv4Addr::LOCALHOST));
        assert!(!is_usable_v4(&Ipv4Addr::UNSPECIFIED));
        assert!(is_usable_v4(&Ipv4Addr::new(192, 168, 1, 134)));
        assert!(is_usable_v4(&Ipv4Addr::new(10, 2, 0, 2)));
    }

    #[test]
    fn ipv6_link_local_is_not_usable() {
        assert!(!is_usable_v6(&"fe80::1".parse().unwrap()));
        assert!(!is_usable_v6(&"febf::1".parse().unwrap()));
        assert!(!is_usable_v6(&Ipv6Addr::LOCALHOST));
        assert!(is_usable_v6(&"2001:db8::1".parse().unwrap()));
        assert!(is_usable_v6(&"fd00::1".parse().unwrap()));
    }

    #[test]
    fn if_type_fallback_maps_the_two_kinds_we_care_about() {
        assert_eq!(kind_from_if_type(IF_TYPE_ETHERNET_CSMACD), NicKind::Ethernet);
        assert_eq!(kind_from_if_type(IF_TYPE_IEEE80211), NicKind::Wifi);
        assert_eq!(kind_from_if_type(24), NicKind::Other); // loopback
        assert_eq!(kind_from_if_type(131), NicKind::Other); // tunnel
    }

    fn nic(live: bool, tier: Tier, gw: bool, speed: u64) -> Nic {
        Nic {
            luid: LuidKey(speed.max(1)),
            if_index: 1,
            friendly_name: "x".into(),
            description: "x".into(),
            adapter_name: "{x}".into(),
            if_type: IF_TYPE_ETHERNET_CSMACD,
            kind: NicKind::Ethernet,
            tier,
            oper_up: live,
            media_connected: live,
            tx_speed_bps: Some(speed),
            mac: None,
            permanent_mac: None,
            ipv4: if live { vec![Ipv4Addr::new(192, 168, 1, 2)] } else { vec![] },
            ipv6: vec![],
            gateways: if gw { vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))] } else { vec![] },
        }
    }

    #[test]
    fn ranking_prefers_live_hardware_with_a_gateway() {
        let good = nic(true, Tier::Hardware, true, 1_000_000_000);
        let dead = nic(false, Tier::Hardware, true, 10_000_000_000);
        let virt = nic(true, Tier::Virtualised, true, 10_000_000_000);
        let no_gw = nic(true, Tier::Hardware, false, 10_000_000_000);
        assert!(rank(&good) < rank(&dead), "live beats faster-but-dead");
        assert!(rank(&good) < rank(&virt), "hardware beats virtualised");
        assert!(rank(&good) < rank(&no_gw), "having a gateway wins");
    }

    #[test]
    fn ranking_breaks_ties_on_speed() {
        let fast = nic(true, Tier::Hardware, true, 2_500_000_000);
        let slow = nic(true, Tier::Hardware, true, 100_000_000);
        assert!(rank(&fast) < rank(&slow));
    }
}
