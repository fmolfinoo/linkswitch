# LinkSwitch — Environment Addendum to the Implementation Spec

**Scope:** the parts of the design that exist *because of what is actually on `DEV-PC`* (Win11 Education 10.0.26200, Intel I225‑V + RZ608 Wi‑Fi 6E, ProtonVPN 5.1.7 / WireGuardNT, VMware Workstation, Hyper‑V Default Switch, Tailscale/Wintun installed).
**Status of the numbers below:** re-measured read-only at authoring time (2026‑09‑07). Nothing on this machine was modified. Where the three research dives disagreed with their own verification passes, **the verification wins and is what is written here** — the dives' `MediaType == NdisMediumNative802_11` test, `BOOLEAN(0)` field writes, `GetAdaptersAddresses`-based enumeration, and "write `prior` verbatim" restore rule are all *rejected* and replaced.

Live snapshot used throughout:

```
ifIndex 4   Ethernet   I225-V     Disconnected  MAC AA-BB-CC-DD-EE-01  IPv4 metric 5    Auto=Enabled   169.254.10.20/16 (APIPA, Tentative)
ifIndex 14  Wi-Fi      RZ608      Up            MAC AA-BB-CC-DD-EE-02  IPv4 metric 30   Auto=Enabled   192.168.1.51/24  gw 192.168.1.1
ifIndex 43  ProtonVPN  WireGuard  Up            no MAC                 IPv4 metric 0    Auto=Disabled  10.0.0.2/32
routes: 43 0.0.0.0/0 metric 0 NetMgmt | 14 0.0.0.0/0 -> 192.168.1.1 metric 0 | 14 203.0.113.10/32 -> 192.168.1.1 metric 30 NetMgmt  <-- Proton's endpoint pin
ms_tcpip6 = False on Ethernet, Wi-Fi, ProtonVPN, VMnet1, VMnet8, Bluetooth; True only on vEthernet (Default Switch)
22 adapters via Get-NetAdapter -IncludeHidden; 53 interfaces via GetIfTable2
```

---

## 1. Adapter classification

### 1.1 Decisions

| Decision | Value | Why |
|---|---|---|
| Enumeration source | **`GetIfTable2`**, never `GetAdaptersAddresses` | GAA omits an adapter whose protocol is unbound for the requested family. Measured proof: `AF_INET6` + default flags returns **2** adapters (loopback + vEthernet — exactly the two with `ms_tcpip6=True`); `AF_UNSPEC` returns 10; `AF_INET6 + GAA_FLAG_INCLUDE_ALL_INTERFACES` returns 53. Mode 3 unbinds `ms_tcpip` on Ethernet — with GAA the app would **lose sight of the NIC it just silenced** and be unable to offer "restore". `GetIfTable2` returned all 53 NDIS interfaces regardless of binding. |
| Gate | flags + `OperStatus != IfOperStatusNotPresent` + `(Type, PhysicalMediumType)` | Flags alone admit **four** interfaces here, not two (idx 13 "Ethernet 2" and idx 21 "Ethernet 4", both UsbNcm Host Device, both `_bitfield == 0x05`). They are excluded only by `OperStatus == 6 (NotPresent)`. That gate is **load-bearing**, not decorative. |
| `MediaType` test | **removed** | `MIB_IF_ROW2.MediaType` for the real Wi‑Fi radio (idx 14) is **`NdisMedium802_3` (0)**, not `NdisMediumNative802_11` (16). The 16 comes from WMI's `MSFT_NetAdapter.NdisMedium` (miniport view); iphlpapi sees the Native‑WiFi 802.3 emulation. A classifier that tests `MediaType == 16` matches **only Ethernet** and silently makes modes 2 and 3 unreachable. Verified by compiling and running the original function over the live table: `total matches: 1`. |
| Cargo features | `Win32_Foundation`, `Win32_NetworkManagement_IpHelper`, **`Win32_NetworkManagement_Ndis`**, **`Win32_Networking_WinSock`** | `MIB_IPINTERFACE_ROW`, `GetIpInterfaceEntry`, `SetIpInterfaceEntry` are gated on `all(Ndis, WinSock)` — *not* on IpHelper. Omitting them yields `no MIB_IPINTERFACE_ROW in Win32::NetworkManagement::IpHelper`, which reads like a hallucinated path. Add a comment in `Cargo.toml` so nobody prunes them as unused. |

### 1.2 Final classifier (compiles against `windows` 0.62.2)

```rust
// src/net/classify.rs
use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::{
    IfOperStatusNotPresent, IfOperStatusUp,
    NdisPhysicalMedium802_3, NdisPhysicalMediumNative802_11,
    NdisPhysicalMediumUnspecified, NdisPhysicalMediumWirelessLan,
};

// windows-rs exposes MIB_IF_ROW2.InterfaceAndOperStatusFlags as
// `MIB_IF_ROW2_0 { pub _bitfield: u8 }` — no named accessors. Mask it ourselves.
// C declaration order (ns-netioapi-mib_if_row2), MSVC LSB-first packing:
pub const F_HARDWARE_INTERFACE:  u8 = 0x01;
pub const F_FILTER_INTERFACE:    u8 = 0x02;
pub const F_CONNECTOR_PRESENT:   u8 = 0x04;
pub const F_NOT_AUTHENTICATED:   u8 = 0x08;
pub const F_NOT_MEDIA_CONNECTED: u8 = 0x10;
pub const F_PAUSED:              u8 = 0x20;
pub const F_LOW_POWER:           u8 = 0x40;
pub const F_END_POINT_INTERFACE: u8 = 0x80;

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211:       u32 = 71;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LinkKind { Ethernet, WiFi }

/// The single source of truth for "is this a steerable physical link?".
/// Validated against all 53 rows GetIfTable2 returns on this machine:
/// emits exactly ifIndex 4 (Ethernet) and ifIndex 14 (Wi-Fi).
pub fn classify(r: &MIB_IF_ROW2) -> Option<LinkKind> {
    let f = r.InterfaceAndOperStatusFlags._bitfield;

    if f & F_HARDWARE_INTERFACE == 0 { return None; } // VMnet(0x80), vEthernet(0x00), ProtonVPN(0x00),
                                                      // Bluetooth PAN(0x10), Wi-Fi Direct(0x10),
                                                      // WAN miniports(0x00/0x10), Tailscale(0x00)
    if f & F_CONNECTOR_PRESENT  == 0 { return None; }
    if f & F_END_POINT_INTERFACE != 0 { return None; } // phone-tether / RNDIS endpoints
    if f & F_FILTER_INTERFACE    != 0 { return None; } // the 22 LWF modules (0x02 / 0x12)

    // MANDATORY: without this, widening Ethernet to PhysicalMediumUnspecified
    // admits ifIndex 13 and 21 (UsbNcm Host Device), which pass every flag test.
    if r.OperStatus == IfOperStatusNotPresent { return None; }

    match r.Type {
        IF_TYPE_ETHERNET_CSMACD
            if r.PhysicalMediumType == NdisPhysicalMedium802_3
            || r.PhysicalMediumType == NdisPhysicalMediumUnspecified => Some(LinkKind::Ethernet),

        IF_TYPE_IEEE80211
            if r.PhysicalMediumType == NdisPhysicalMediumNative802_11
            || r.PhysicalMediumType == NdisPhysicalMediumWirelessLan => Some(LinkKind::WiFi),

        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Link {
    pub kind: LinkKind,
    pub if_index: u32,          // in-process handle ONLY. Never persisted.
    pub luid: u64,              // in-process handle ONLY. Never persisted. (union read -> unsafe)
    pub guid: windows::core::GUID,
    pub alias: String,          // "Ethernet" / "Wi-Fi"
    pub description: String,    // "Intel(R) Ethernet Controller (3) I225-V"
    pub permanent_mac: [u8; 6], // <-- DURABLE IDENTITY KEY
    pub oper_up: bool,
    pub media_connected: bool,  // !(flags & F_NOT_MEDIA_CONNECTED)
}

pub fn enumerate_links() -> Result<Vec<Link>, windows::core::Error> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    let rc = unsafe { GetIfTable2(&mut table) };
    if rc != NO_ERROR { return Err(windows::core::Error::from(rc.to_hresult())); }

    let out = unsafe {
        let t = &*table;
        let rows = std::slice::from_raw_parts(t.Table.as_ptr(), t.NumEntries as usize);
        rows.iter().filter_map(|r| {
            let kind = classify(r)?;
            if r.PhysicalAddressLength != 6 { return None; } // defensive: a real NIC has a 6-byte MAC
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&r.PermanentPhysicalAddress[..6]);
            Some(Link {
                kind,
                if_index: r.InterfaceIndex,
                luid: r.InterfaceLuid.Value,          // union field -> requires unsafe
                guid: r.InterfaceGuid,
                alias: wstr(&r.Alias),
                description: wstr(&r.Description),
                permanent_mac: mac,
                oper_up: r.OperStatus == IfOperStatusUp,
                media_connected: r.InterfaceAndOperStatusFlags._bitfield & F_NOT_MEDIA_CONNECTED == 0,
            })
        }).collect()
    };
    unsafe { FreeMibTable(table as *const _) };   // the dives never freed the table
    Ok(out)
}
```

**Startup self-test (ship it, run it once at worker and UI start):**

```rust
// Guards against a windows-rs bitfield layout change silently inverting the gate.
debug_assert!(enumerate_links().unwrap().iter().any(|l| l.kind == LinkKind::WiFi),
              "classifier found no Wi-Fi link — see ENV-ADDENDUM §1.1 (MediaType trap)");
// Ethernet row today: _bitfield == 0x15 (HW|CONN|NotMediaConnected, because the cable is out)
// Wi-Fi   row today: _bitfield == 0x05 (HW|CONN)
// Assert the invariant part only:  f & 0x85 == 0x05  for every classified link.
```

### 1.3 Verdict for every adapter on this machine

| ifIndex | Name | Type | `_bitfield` | OperStatus | PhysMedium | **Verdict** | Rejected by |
|---|---|---|---|---|---|---|---|
| 4 | Ethernet (I225‑V) | 6 | `0x15` | 2 Down | 14 (802_3) | **`Ethernet`** ✅ | — |
| 14 | Wi‑Fi (RZ608) | 71 | `0x05` | 1 Up | 9 (Native802_11) | **`WiFi`** ✅ | — |
| 19 / 23 | VMware VMnet8 / VMnet1 | 6 | `0x80` | Up | 14 | `None` | HW=0, Conn=0, EP=1 |
| 39 | vEthernet (Default Switch) | 6 | `0x00` | Up | 0 | `None` | HW=0 |
| 37 | vSwitch (Default Switch) | 6 | `0x00` | Up | — | `None` | HW=0 |
| 6 | Bluetooth PAN | 6 | `0x10` | Down | 10 (Bluetooth) | `None` | HW=0 |
| 16 / 17 | Wi‑Fi Direct Virtual #2 / #1 | 71 | `0x10` | Down | 9 (**identical to real radio**) | `None` | HW=0, Conn=0 |
| 43 | **ProtonVPN** (WireGuard) | 53 | `0x00` | Up | 0 | `None` | HW=0 |
| 9 | **Tailscale** (Wintun) | 53 | `0x00` | 2 Down | 0 | `None` | HW=0 |
| 2 / 13 / 21 | Apple Mobile Device Eth / UsbNcm ×2 | 6 | `0x01` / `0x05` / `0x05` | **6 NotPresent** | 0 | `None` | **OperStatus gate only** |
| 5 / 8 / 22 | WAN Miniport IP / NetMon / IPv6 | 6 | `0x00` | Up | — | `None` | HW=0 |
| 10 / 11 / 15 / 20 | WAN Miniport PPTP/SSTP/IKEv2/L2TP | 131 | `0x10` | Down | — | `None` | HW=0, Type |
| 24 | WAN Miniport PPPOE | 23 | `0x10` | Down | 12 (CoWan) | `None` | HW=0, Type |
| 3 / 7 / 18 | 6to4 / IP‑HTTPS / Teredo | 131 | `0x00` | NotPresent | — | `None` | HW=0 |
| 12 | Ethernet (Kernel Debugger) | 6 | `0x00` | NotPresent | — | `None` | HW=0 |
| 1 | Loopback Pseudo‑Interface 1 | 24 | `0x00` | Up | — | `None` | HW=0, Type |
| 25–32, +14 more | LWF filter modules (`Ethernet-QoS Packet Scheduler-0000`, `Wi-Fi-WFP Native MAC Layer…`) | 6 / 71 | `0x02` / `0x12` | — | mirrors host NIC | `None` | **FilterInterface** |

> ⚠️ **The 22 filter-module rows carry the *same* `PermanentPhysicalAddress` as the NIC they sit on** (`AABBCCDDEE01` appears on idx 4, 25, 26, 27; `AABBCCDDEE02` on idx 14 and five more). Identity resolution by MAC must therefore run **only over rows that already passed `classify()`**, never over the raw table.

### 1.4 Durable identity key

**Persist `permanent_mac` (6 bytes, from `MIB_IF_ROW2.PermanentPhysicalAddress`). Nothing else is a key.**

```json
{ "role": "Ethernet",
  "permanent_mac": "AA-BB-CC-DD-EE-01",
  "cache_guid": "{11111111-2222-3333-4444-555555555555}",
  "cache_description": "Intel(R) Ethernet Controller (3) I225-V" }
```

Resolution order at every operation (never cached across an operation):
1. `cache_guid` string match against today's classified links → O(1) hit.
2. Miss → scan classified links of the saved `role` for a full 6‑byte `permanent_mac` match; on hit, rewrite `cache_guid`.
3. Miss → the NIC was removed/replaced. Show `cache_description` as a *suggestion* and **require the user to re-pick**. Never auto-bind.

Why not the alternatives:

* **`ifIndex` — rejected.** Documented non-persistent: *"may change when a network adapter is disabled and then enabled."* The device-disable flavour of mode 3 is precisely that event.
* **`NET_LUID` — rejected as a key** (the dive's reasoning was also wrong, in a way that matters). A LUID is a *reusable slot number*, not a device identity: Ethernet is `0x0006008000000000` and Wi‑Fi is `0x0047008000000000` — **both have `NetLuidIndex == 0x008000`**, i.e. "first miniport of this IfType". Replace the I225‑V and the new NIC very plausibly gets the byte-identical LUID. Use LUID only as the in-process argument to `Get/SetIpInterfaceEntry`.
* **`InterfaceGuid` — cache only.** Stable across reboot/sleep/cable events; regenerated by a driver uninstall+reinstall.
* **`PhysicalAddress` (current MAC) — never.** Wi‑Fi "Random hardware addresses" makes it drift; `PermanentPhysicalAddress` does not. Also compare **all 6 bytes**: Bluetooth PAN here is `AA-BB-CC-DD-EE-03`, one off from Wi‑Fi's `…-7D`, and the Wi‑Fi Direct pseudo-NICs are `AC-BB-CC-DD-EE-04` / `AC-BB-CC-DD-EE-05` — derived from the radio's MAC.

---

## 2. IPv6 is absent — exact handling

### 2.1 The fact

`ms_tcpip6` is unbound on **every physical adapter**. Consequently:

```
GetIpInterfaceEntry(Family = AF_INET6, InterfaceIndex = 4 | 14 | 43 | 19 | 23)
    -> WIN32_ERROR(1168)  ERROR_NOT_FOUND  "Element not found"
GetIpInterfaceEntry(Family = AF_INET6, InterfaceIndex = 39 | 1)          -> NO_ERROR   (positive control)
GetIpInterfaceEntry(Family = AF_INET | AF_INET6, InterfaceIndex = 9999)  -> WIN32_ERROR(2) ERROR_FILE_NOT_FOUND
```

MS Learn defines `ERROR_NOT_FOUND` for this call as: *"returned if the network interface specified by the InterfaceLuid or InterfaceIndex member … does not match the IP address family specified in the Family member."* That is exactly "family not bound on this interface".

**Triage is total and unambiguous:**

| rc | Meaning | Action |
|---|---|---|
| `0` | Row exists, fields meaningful | proceed |
| **`1168`** | **Family not bound on an interface that exists** | **not an error.** Skip / render "not present". Never a toast, never red, never retried. |
| `2` | Adapter itself is gone (index recycled / removed) | re-resolve by MAC; if that fails, drop from model |
| `5` | Not elevated (only ever from `Set…`) | worker mis-launch → distinct error |
| `87` | Our row is malformed | **our bug**; log loudly, no retry |

**`1168` is not just an IPv6 code.** Once mode 3 unbinds `ms_tcpip`, `GetIpInterfaceEntry(AF_INET, ifIndex 4)` will return `1168` too. This is already observable today on adapters that exist but have no IPv4 stack — measured: **ifIndex 37, 12, 3, 18, 7, 5, 8 all return `1168` for `AF_INET`, never `2`**. A poller that treats any non-zero rc as failure shows a permanent red state *in the mode the app is designed to produce*.

### 2.2 The match arm

```rust
// src/net/ipiface.rs
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND,
    NO_ERROR, WIN32_ERROR,
};
use windows::Win32::NetworkManagement::IpHelper::{GetIpInterfaceEntry, MIB_IPINTERFACE_ROW};
use windows::Win32::Networking::WinSock::{ADDRESS_FAMILY, AF_INET, AF_INET6};

pub enum FamilyState {
    Present(Box<MIB_IPINTERFACE_ROW>),
    /// The adapter exists; this protocol is not bound to it.
    /// TODAY: every physical NIC, AF_INET6. IN MODE 3: Ethernet, AF_INET.
    NotBound,
}

pub enum IfaceError { AdapterGone, AccessDenied, BadRow, NotApplied, Other(WIN32_ERROR) }

pub fn read_ip_iface(luid: u64, family: ADDRESS_FAMILY) -> Result<FamilyState, IfaceError> {
    let mut row = MIB_IPINTERFACE_ROW::default();
    row.Family = family;
    row.InterfaceLuid.Value = luid;          // union write -> inside unsafe in real code

    // windows-rs returns WIN32_ERROR directly. Do NOT call GetLastError() after this.
    let rc = unsafe { GetIpInterfaceEntry(&mut row) };

    match rc {
        NO_ERROR                => Ok(FamilyState::Present(Box::new(row))),
        ERROR_NOT_FOUND         => Ok(FamilyState::NotBound),   // <-- 1168. THE ARM. Never an Err.
        ERROR_FILE_NOT_FOUND    => Err(IfaceError::AdapterGone),
        ERROR_ACCESS_DENIED     => Err(IfaceError::AccessDenied),
        ERROR_INVALID_PARAMETER => Err(IfaceError::BadRow),
        e                       => Err(IfaceError::Other(e)),
    }
}

/// Enumerate only families that actually exist. On this machine: [AF_INET] for
/// Ethernet and Wi-Fi — never [AF_INET, AF_INET6]. Drives the UI's per-family rows.
pub fn live_families(luid: u64) -> Vec<ADDRESS_FAMILY> {
    [AF_INET, AF_INET6].into_iter()
        .filter(|f| matches!(read_ip_iface(luid, *f), Ok(FamilyState::Present(_))))
        .collect()
}
```

### 2.3 Consequences that are design, not trivia

* **The UI never shows an IPv6 row for Ethernet or Wi‑Fi.** It shows, once, in the status pane:
  `IPv6 is not bound on this PC's Ethernet or Wi-Fi adapters. LinkSwitch has nothing to steer for IPv6 and will not change that.`
* **LinkSwitch never writes `ms_tcpip6 = Enabled`. Ever.** See §3.6 — the only `enable_binding` call site in the codebase is inside `restore_entry`, and it is unreachable for an entry with `we_changed_it == false`.
* **`Get-NetIPInterface` for an absent family writes a *non-terminating* `ObjectNotFound` error** (`FullyQualifiedErrorId = CmdletizationQuery_NotFound,Get-NetIPInterface`) and returns `$null`. It becomes fatal only under `$ErrorActionPreference = 'Stop'`. The rescue script in §5 therefore does **not** set a global `Stop`.
* **Registry `Tcpip6\Linkage\Bind` is forensics, not state.** It lists 4 devices here — 6to4 (idx 3), Teredo (18), IP‑HTTPS (7), vEthernet Default Switch (39) — and **three of those four return `1168` for `AF_INET6`**. Bind-list membership is necessary, not sufficient. Never cross-check journal state against it; you will manufacture false conflicts.

---

## 3. "Never enable what we did not disable" — the state model

**This section is correctness-critical. Deviations are bugs, not style.**

### 3.1 Storage

* File: `%ProgramData%\LinkSwitch\journal.json` (ACL: SYSTEM + Administrators full, Users read).
* Written **only** by the elevated worker. Read by both processes.
* Every write is: serialize → `journal.json.tmp` → `FlushFileBuffers` → `ReplaceFileW` → `FlushFileBuffers` on the directory handle. No partial states on disk.
* Second file: `%ProgramData%\LinkSwitch\pending_revert.json` — the armed-revert marker (§3.7). Written **before** any `INetCfg::Apply`.

### 3.2 Entry format

```json
{
  "schema": 1,
  "app_version": "0.1.0",
  "session": "7c1f4b2e-4d33-4c0e-9a5a-2ba1b1e7f001",
  "created_utc": "2026-09-07T18:22:03Z",
  "mode_requested": "WifiOnly",
  "mode_effective": "WifiOnly",
  "worker_pid": 20144,
  "entries": [
    {
      "id": "e1",
      "kind": "Binding",
      "adapter": {
        "role": "Ethernet",
        "permanent_mac": "AA-BB-CC-DD-EE-01",
        "guid_at_capture": "{11111111-2222-3333-4444-555555555555}",
        "alias_at_capture": "Ethernet",
        "pnp_id_at_capture": "PCI\\VEN_8086&DEV_15F3&SUBSYS_87D21043&REV_03\\C87F54FFFF0711AA00"
      },
      "component_id": "ms_tcpip",
      "prior":   { "enabled": true },
      "desired": { "enabled": false },
      "we_changed_it": true,
      "phase": "applied",
      "applied_utc": "2026-09-07T18:22:04Z"
    },

    {
      "id": "e2",
      "kind": "Binding",
      "adapter": { "role": "Ethernet", "permanent_mac": "AA-BB-CC-DD-EE-01", "...": "..." },
      "component_id": "ms_tcpip6",
      "prior":   { "enabled": false },
      "desired": { "enabled": false },
      "we_changed_it": false,
      "phase": "noop",
      "note": "Already disabled by a third party before LinkSwitch ran (VPN IPv6 leak protection is the usual cause, unproven here). LinkSwitch did not disable it and must never enable it."
    },

    {
      "id": "e3",
      "kind": "IpInterface",
      "adapter": { "role": "Ethernet", "permanent_mac": "AA-BB-CC-DD-EE-01", "...": "..." },
      "family": "IPv4",
      "prior":   { "metric_mode": "Automatic" },
      "desired": { "metric_mode": "Manual", "metric": 9000 },
      "observed_at_capture": {
        "metric": 5,
        "nl_mtu": 1500,
        "site_prefix_length": 64,
        "disable_default_routes": false,
        "weak_host_send": false,
        "weak_host_receive": false,
        "forwarding_enabled": false
      },
      "we_changed_it": true,
      "phase": "applied"
    },

    {
      "id": "e4",
      "kind": "IpInterface",
      "adapter": { "role": "Ethernet", "permanent_mac": "AA-BB-CC-DD-EE-01", "...": "..." },
      "family": "IPv6",
      "prior": null, "desired": null,
      "we_changed_it": false,
      "phase": "skipped",
      "skip_reason": "GetIpInterfaceEntry returned ERROR_NOT_FOUND (1168): family not bound"
    },

    {
      "id": "e5",
      "kind": "BindingTableSnapshot",
      "adapter": { "role": "Ethernet", "permanent_mac": "AA-BB-CC-DD-EE-01", "...": "..." },
      "when": "pre",
      "table": { "ms_msclient": true, "vmware_bridge": true, "ms_lldp": true, "ms_tcpip6": false,
                 "ms_tcpip": true, "ms_implat": false, "ms_rspndr": true, "ms_lltdio": true,
                 "vms_pp": false, "ms_server": true, "ms_l2bridge": true, "ms_l1vhlwf": true,
                 "ms_pacer": true }
    },

    {
      "id": "e6",
      "kind": "Device",
      "adapter": { "role": "Ethernet", "permanent_mac": "AA-BB-CC-DD-EE-01", "...": "..." },
      "prior":   { "enabled": true },
      "desired": { "enabled": true },
      "we_changed_it": false,
      "phase": "noop",
      "note": "Device-disable variant not used; default mode-3 mechanism is the ms_tcpip unbind."
    }
  ]
}
```

**Mandated field semantics — non-negotiable:**

| Field | Rule |
|---|---|
| `we_changed_it` | The whole safety model. `false` ⇒ restore returns `NotOurs` and **writes nothing**, no flag, no `--force`, no override. |
| `prior` for metrics | **Semantic, never raw fields**: `{"metric_mode":"Automatic"}` or `{"metric_mode":"Manual","metric":N}`. The dives' `{"use_automatic_metric":true,"metric":5}` is *rejected*: (a) "always write prior" would then write `metric=5` with auto off, permanently pinning an interface whose 5 is the **automatic** value for a 2.5 GbE link; (b) after a correct automatic restore the stack recomputes to 5, so a raw-field comparison can never match and every entry misclassifies as `Abandoned`. |
| `observed_at_capture` | Display + forensics only. **Never** written back. Includes `disable_default_routes` etc. so a post-hoc diff can prove we didn't clobber a VPN setting. |
| `nl_mtu` | Not in `prior`. `set_metric` is read-modify-write, so MTU is preserved implicitly. Storing it invites someone to write it back. |
| `phase` | `pending → applied` \| `noop` \| `skipped` \| `failed` \| `reverted` \| `abandoned`. |
| Adapter identity | `permanent_mac` is the key. `guid_at_capture` / `alias_at_capture` / `pnp_id_at_capture` are re-resolution hints and display strings. |

### 3.3 Capture procedure (runs immediately before every change)

1. `enumerate_links()` → resolve the target `Link` by saved MAC. **Re-read live every time.** Cached state is worthless here: `HKLM\...\Tcpip6\Linkage` was last written **27 seconds after boot**, and both `Tcpip` and `Tcpip6` Bind lists are rewritten on **every ProtonVPN connect/disconnect** as the WireGuard adapter appears and disappears.
2. `read_binding_table(link)` → full 13-component map → append `BindingTableSnapshot{when:"pre"}`.
3. For each family in `live_families(luid)`: `read_ip_iface` → build `prior` (semantic) + `observed_at_capture`. For a family that returns `1168`, append `phase:"skipped"` with the `skip_reason` above.
4. Compute `we_changed_it` **from live state, not from intent**: `prior != desired` ⇒ `true`; equal ⇒ `false` and `phase:"noop"` — *this is the `ms_tcpip6` path on this machine, and it is the reason the user's IPv6 leak posture survives an uninstall, a rescue run, and a crash.*
5. Append `pending` entries → **fsync** → *only then* perform any change.

### 3.4 Apply procedure

```rust
// The whole-program invariant: `enable_binding()` is `pub(in crate::restore)` and has
// EXACTLY ONE call site, inside restore_entry(). The apply path is structurally
// incapable of enabling a binding. CI greps for `Enable-NetAdapterBinding` and for
// `enable_binding(` outside src/restore/ and fails the build on a hit.
fn apply_disable_binding(j: &mut Journal, link: &Link, cid: &str) -> Result<()> {
    let prior = read_binding(link, cid)?;
    if !prior {
        j.push(Entry::binding_noop(link, cid, prior)); j.flush_fsync()?;
        return Ok(());                                   // ms_tcpip6 takes this branch here
    }
    let pre = read_binding_table(link)?;
    j.push(Entry::snapshot(link, "pre", &pre));
    j.push(Entry::binding_pending(link, cid, prior, false));
    j.flush_fsync()?;                                    // journal BEFORE the write. Always.

    write_pending_revert(link, cid, prior)?;             // §3.7, fsync'd, BEFORE INetCfg::Apply
    disable_binding(link, cid)?;                         // INetCfgComponentBindings::UnbindFrom

    let post = read_binding_table(link)?;
    for (other, now) in post.iter() {
        if other != cid && pre[other] != *now {
            // NDIS documents this: "Disabling some adapter bindings can automatically
            // enable other network adapter bindings."
            j.push(Entry::binding_collateral(link, other, pre[other], *now));
            if other == "ms_tcpip6" && *now {
                // P0. Revert immediately, journal it, tell the user.
                disable_binding(link, "ms_tcpip6")?;
                j.push(Entry::p0_collateral_reverted(link, "ms_tcpip6"));
            }
        }
    }
    j.push(Entry::snapshot(link, "post", &post));
    j.mark_applied(link, cid); j.flush_fsync()?;
    Ok(())
}
```

**Metric write — the two traps and the guard the dives missed:**

```rust
pub enum MetricTarget { Automatic, Manual(u32) }

pub fn set_metric(link: &Link, family: ADDRESS_FAMILY, t: MetricTarget)
    -> Result<(), IfaceError>
{
    // Read-modify-write ONLY. Never InitializeIpInterfaceEntry + partial fill:
    // MS Learn warns "the only way to determine all of the fields being changed would be
    // to compare the fields ... with fields set by the InitializeIpInterfaceEntry function",
    // and that path would write DisableDefaultRoutes = FALSE — silently switching off a
    // VPN client's split-tunnel protection. DisableDefaultRoutes is NOT in the
    // ignored-on-input list and IS documented as the VPN security member.
    let mut row = match read_ip_iface(link.luid, family)? {
        FamilyState::Present(r) => *r,
        FamilyState::NotBound   => return Err(IfaceError::NotApplied), // NOT Ok(()) — see below
    };
    let ddr_before = row.DisableDefaultRoutes;    // bool in windows 0.62.2

    match t {
        // windows 0.62.2 maps C BOOLEAN -> Rust `bool`. There is NO `windows::Win32::Foundation::BOOLEAN`;
        // `BOOLEAN(0)` does not compile (E0432). The dives had this exactly inverted.
        MetricTarget::Manual(m) => { row.UseAutomaticMetric = false; row.Metric = m; }
        MetricTarget::Automatic => { row.UseAutomaticMetric = true;  row.Metric = 0; }
    }
    if family == AF_INET {
        // Get hands back SitePrefixLength = 64 on IPv4 rows (measured: ifIndex 4, 14, 19, 23,
        // loopback). Docs: ">32 is an illegal value" for IPv4 and "must be set to 0" on Set.
        // The exact failure code if you skip this is UNVERIFIED (we never wrote) — so log the
        // raw WIN32_ERROR from the first Set the product ever performs.
        row.SitePrefixLength = 0;
    }

    let rc = unsafe { SetIpInterfaceEntry(&mut row) };
    match rc {
        NO_ERROR                => {}
        ERROR_NOT_FOUND         => return Err(IfaceError::NotApplied), // family vanished mid-apply
        ERROR_FILE_NOT_FOUND    => return Err(IfaceError::AdapterGone),
        ERROR_ACCESS_DENIED     => return Err(IfaceError::AccessDenied),
        ERROR_INVALID_PARAMETER => { log::error!("SetIpInterfaceEntry rc=87 raw={:?}", rc);
                                     return Err(IfaceError::BadRow); }
        e                       => return Err(IfaceError::Other(e)),
    }

    // TOCTOU guard: if the VPN armed DisableDefaultRoutes between our Get and our Set,
    // we just wrote the stale FALSE back. Detect and repair.
    if let Ok(FamilyState::Present(after)) = read_ip_iface(link.luid, family) {
        if after.DisableDefaultRoutes != ddr_before {
            log::error!("P0: DisableDefaultRoutes changed across our write ({} -> {})",
                        ddr_before, after.DisableDefaultRoutes);
            // Do not silently "fix" it — surface STR-P0-DDR and abort the mode transition.
            return Err(IfaceError::BadRow);
        }
    }
    Ok(())
}
```

> **`ERROR_NOT_FOUND` from `Set…` must be `NotApplied`, never `Ok(())`.** Returning success there journals `phase:"applied"`, `we_changed_it:true`, `desired:{Manual,9000}` for a write that did not happen. Every later restore then sees `live != desired` → `Abandoned`, the entry never restores, and the UI raises a permanent phantom "something else changed this" — while telling the user Wi‑Fi mode is active with Ethernet still winning the route.

### 3.5 Restore procedure

```rust
pub enum Restore {
    Restored,
    NotOurs,                                                  // we_changed_it == false
    AlreadyPrior,                                             // someone/reboot undid it for us
    Abandoned { expected: Value, found: Value, prior: Value }, // live != desired: DO NOT WRITE
    RefusedByRatchet { found: Value, prior: Value },
    NotApplicable,                                            // family/adapter gone
}

pub fn restore_entry(e: &Entry) -> Result<Restore, IfaceError> {
    // GUARD 1 — the hard rule. No exceptions, no flags, no --force, no admin override.
    if !e.we_changed_it { return Ok(Restore::NotOurs); }

    let live = match read_live(e) { Ok(Some(v)) => v, Ok(None) => return Ok(Restore::NotApplicable),
                                    Err(x) => return Err(x) };

    if semantically_equal(&live, &e.prior) { return Ok(Restore::AlreadyPrior); }

    if !semantically_equal(&live, &e.desired) {
        // Someone changed it after us: VPN re-applied leak protection, driver reinstall,
        // GPO, or the user edited adapter properties. We cannot know whose intent is newer.
        // Writing `prior` here can clobber a deliberate hardening. DO NOT WRITE.
        return Ok(Restore::Abandoned { expected: e.desired.clone(), found: live,
                                        prior: e.prior.clone() });
    }

    // GUARD 2 — the security ratchet. Restore may relax OUR change; it may never be the
    // thing that re-enables a protocol, or clears a DisableDefaultRoutes, that another
    // actor set. Belt-and-braces behind Guard 1.
    if reduces_isolation(&live, &e.prior) && !we_are_sole_writer(e)? {
        return Ok(Restore::RefusedByRatchet { found: live, prior: e.prior.clone() });
    }

    write_live(e, &e.prior)?;   // writes PRIOR, semantically. Never a literal `true`/`Enabled`.
    Ok(Restore::Restored)
}
```

`semantically_equal` for metric entries compares **`metric_mode` first**, and the number only in the `Manual` arm. `Automatic` vs `Automatic` matches regardless of the recomputed value (5 / 30 here). `reduces_isolation` is true for: enabling any binding, clearing `DisableDefaultRoutes`, clearing `ForwardingEnabled`… — i.e. any transition toward *more* connectivity.

### 3.6 Conflict resolution — the decision table

| Live vs journal | `we_changed_it` | Outcome | Writes? | UI |
|---|---|---|---|---|
| any | `false` | `NotOurs` | **no** | silent (it's not ours to touch) |
| `live == prior` | `true` | `AlreadyPrior` | no | mark reverted, silent |
| `live == desired` | `true` | `Restored` | **yes, `prior`** | normal |
| `live != desired`, `live != prior` | `true` | `Abandoned` | **no** | one-click card, §6 STR‑CONFLICT |
| restore would relax isolation set by another actor | `true` | `RefusedByRatchet` | **no** | STR‑RATCHET |

**Abandoned is not an error and is never auto-resolved.** It becomes a persistent card in the UI offering *one* explicit, user-authorised action, showing all three values. Abandoned entries survive restarts and are cleared only by the user acting or dismissing.

### 3.7 Crash / reboot recovery — armed revert

Binding changes are **registry-persistent across reboots**, and Proton's Advanced kill switch keeps its `BlockAllIpv4Network` filters `persistent: true` while its server-IP permit and tunnel-interface permit are `persistent: false`. So a machine that reboots with mode 3 un-reverted comes back with **blocks in place and permits gone** until the Proton service re-applies them. That combination is the app's worst failure mode, so:

1. `pending_revert.json` is written and fsync'd **before** `INetCfg::Apply` — not after. `Apply` rebinds the protocol stack and drops all IP on the adapter for several seconds; a crash inside that window must still be recoverable.
2. The worker arms a **20 s** timer. The UI must call `ConfirmMode3()` within it. The UI never needs network to do so.
3. Timeout ⇒ auto re-bind, journal `phase:"reverted"`, UI shows STR‑AUTOREVERT.
4. The pre-registered scheduled task gets **two triggers: on-demand *and* at startup.** The startup run finds a stale `pending_revert.json` and re-binds before the user can be stranded. A timer alone is insufficient because binding state outlives the process.
5. Journal recovery for any `pending` entry: `live == desired` → promote to `applied`; `live == prior` → demote to `noop` **and set `we_changed_it = false`** (we did not change it, so we must never restore it); otherwise → `abandoned`.

### 3.8 `INetCfg` write-lock contention (the real mode‑3 hazard)

Proton's `NetworkUtil` takes the same machine-wide `INetCfg` write lock on every connect/disconnect (to toggle `ms_tcpip6` across adapters). If our unbind lands mid-reconnect, `INetCfgLock::AcquireWriteLock` returns **the holder's name** and we get `NETCFG_E_NO_WRITE_LOCK`.

* Acquire with a **5000 ms** timeout, `pszwClientDescription = "LinkSwitch"` so Proton's log names us rather than "unknown".
* On failure: surface the returned holder name verbatim (STR‑LOCK), **never force**, never retry more than twice with 2 s backoff.
* Never allow two `INetCfg::Apply()` calls to overlap — a process-wide mutex around the whole acquire/modify/apply/release sequence.

### 3.9 Mode → operations (exact, ordered)

| Mode | Ethernet ops (in order) | Wi‑Fi ops |
|---|---|---|
| **1 — Ethernet** | if journal shows our `ms_tcpip` unbind → restore it, wait for `AF_INET` to reappear (poll `read_ip_iface`, 100 ms, 10 s cap); then `set_metric(AF_INET, Automatic)` | **none, ever** |
| **2 — Wi‑Fi** | if unbound by us → restore + wait as above; then `set_metric(AF_INET, Manual(9000))` | **none, ever** |
| **3 — Wi‑Fi only** | **first** restore the metric entry to `prior` (the row is about to vanish; a dangling metric entry on a non-existent family can never be restored), **then** `ms_tcpip` unbind; `ms_tcpip6` unbind **only if currently Enabled=True** (it is not, here → `noop`) | **none, ever** |

* **Mode 1 does not write metric 5.** Ethernet's 5 *is* the automatic value (`AutomaticMetric = Enabled`). Writing 5 explicitly is a functional no-op that also pins `UseAutomaticMetric = false`, so Windows stops re-deriving it when link speed changes. Restore automatic = `Metric = 0` **and** `UseAutomaticMetric = true`, together.
* **9000 is the correct demotion value**, not 100 or 1000: `vEthernet (Default Switch)` sits at **5000** with `AutomaticMetric Disabled`, so anything ≤ 5000 is ambiguous.
* **LinkSwitch never touches ifIndex 43 (ProtonVPN), 39, 37, 19, 23, 9, or 6.** Every `Set` is scoped to a single resolved `NET_LUID`. A "restore all automatic metrics" sweep would clobber the tunnel's and vEthernet's deliberate `AutomaticMetric = Disabled`.
* **Bindings are only ever changed on the adapter being silenced (Ethernet).** `Disable-NetAdapterBinding`'s own docs end all three examples with *"and restarts the network adapter"* — touching Wi‑Fi's bindings would drop the VPN tunnel and every TCP connection on the link the user is actually using.

---

## 4. VPN behaviour

### 4.1 What is actually there

* **ifIndex 43 `ProtonVPN`** — stock **WireGuardNT kernel driver** (`wireguard.sys` 0.10, WireGuard LLC) driven by Proton's own **unversioned `tunnel.dll`** (no version resource; a Proton-built blob, *not* verifiable as stock upstream). Protocol **UDP**. If the user selects Stealth/TCP/TLS, the data path becomes `wireguard-tunnel-tcp.dll` over **Wintun in user mode** and the kernel `socket.c` analysis below **does not apply** — detect the pin empirically, never hardcode.
* **The endpoint pin** `203.0.113.10/32 → 192.168.1.1 dev 14, RouteMetric 30, Protocol NetMgmt` — created by **Proton**, not WireGuard. WireGuard itself installs only the `0.0.0.0/0` on ifIndex 43 at RouteMetric 0.
* **ifIndex 9 `Tailscale` (Wintun, Down)** — a *second* tunnel adapter exists. Any "is a VPN active?" logic must not assume ProtonVPN is the only `IfType 53` adapter.

### 4.2 Why metric steering does not move the tunnel

Two independent mechanisms stack:

1. **Proton's carrier selection is metric-blind.** `BestInterface::IpAddress()` calls legacy `GetAdaptersInfo` and returns the **first adapter whose first gateway is non-zero** (skipping addresses with `IpMask == 0.0.0.0`, excluding the WireGuard adapter by GUID). The metric is read *afterwards*, only to stamp `RouteConfiguration.Metric`. Enumeration order here puts Ethernet before Wi‑Fi.
2. **WireGuardNT resolves the endpoint by longest-prefix-match first.** `SocketResolvePeerEndpoint` walks `GetIpForwardTable2`, skips its own LUID and any interface not `IfOperStatusUp`, and uses `route.Metric + ipInterface.Metric` **only as a tie-break at equal prefix length**. A `/32` beats every `/0` at any metric. The winner is stamped per-packet into an `IP_PKTINFO` cmsg. The cache is invalidated **only** by `NotifyRouteChange2` — there is no `NotifyIpInterfaceChange` subscription, so a pure metric change may not even invalidate it.

> **Honesty caveat that must be in the code comments:** the headline scenario has **never been observed on this machine** — Ethernet is Disconnected, so today's `/32` correctly sits on Wi‑Fi. The Ethernet-pin direction is extrapolation from source. **Detect the pin at runtime; never hardcode "Proton pins to Ethernet."** Proton's server-route behaviour is behind a server-delivered flag (`VpnConfig.IsWireGuardServerRouteEnabled`); with it off there is no `/32` and metric steering *would* move the tunnel.

### 4.3 Detection (all read-only, all in the **UI** process — no elevation)

```rust
pub struct VpnView {
    pub tunnel: Option<Link0>,      // Up, !steerable_physical, owns 0.0.0.0/0 with best combined metric
    pub endpoint_pin: Option<EndpointPin>, // /32, NextHop != 0.0.0.0, Protocol NetMgmt (3),
                                           // on an interface that classify() accepted
    pub carrier: Option<u32>,       // resolve_tunnel_carrier(): faithful port of SocketResolvePeerEndpoint
    pub carrier_is_selected_link: bool,
    pub killswitch_hint: bool,      // service "ProtonVPNCallout" state == Running  (unelevated proxy)
    pub killswitch_confirmed: Option<KillSwitch>, // elevated worker: FwpmEngineOpen0 + FwpmFilterEnum0
    pub nrpt_global_rule: bool,     // Get-DnsClientNrptPolicy namespace "." — live: -> 10.2.0.1
}
```

* `resolve_tunnel_carrier` is the line-for-line port of `SocketResolvePeerEndpoint` (longest prefix first, metric only at equal prefix, skip own LUID, skip `OperStatus != Up`). Fix the compile errors the dive shipped: `GetIpForwardTable2(AF_INET, …)` takes an `ADDRESS_FAMILY` (not `AF_INET.0 as u16`); `MIB_IPINTERFACE_ROW.Family` is `ADDRESS_FAMILY`; **`NET_LUID_LH` and `IfOperStatusUp` live in `::Ndis`, not `::IpHelper`**; `GetIpInterfaceEntry` returns `WIN32_ERROR` (use `.ok()?`, not bare `?`).
* Caveat to display, not hide: WireGuardNT calls `GetIpForwardTable2` **from kernel mode in the tunnel's network compartment**. With WSL2 / Windows Sandbox / Hyper‑V compartments in play the driver's table can differ from ours. Label the computed carrier as *"computed"*, never *"confirmed"*.
* **WFP cannot be read at Medium integrity** (`netsh wfp show filters` → error 5). Kill-switch confirmation is an elevated-worker query; match on **DisplayData names**, never GUIDs (WireGuard's provider/sublayer GUIDs are runtime-generated):
  * `ProtonVPN block IPv4` / `ProtonVPN block IPv6` → kill switch armed
  * `ProtonVPN permit private network` → **absent while blocks present ⇒ LAN is dead in modes 2 *and* 3**, regardless of anything LinkSwitch does
  * Also enumerate the **`WireGuard filters` / `Permissive and blocking filters`** sublayer — `tunnel.dll` runs `blockAll()` with `permitWireGuardService / blockDNS / permitLoopback / permitTunInterface / permitDHCPIPv4 / permitDHCPIPv6 / permitNdp` and **no private-LAN permit**. The dives' detector greps only for `ProtonVPN *` and misses this entirely.
* **Watchdog** — `GetIfEntry2` (`InOctets`/`OutOctets`), *not* `Get-NetAdapterStatistics` (100–300 ms + WinRM plumbing for data we already fetch in microseconds). Declare "stale endpoint pin" only when **all three** hold over a 15 s window: tunnel Tx delta > 64 KiB **AND** tunnel Rx delta == 0 **AND** the computed carrier's own Rx delta > 0.

### 4.4 Per-transition behaviour matrix

Assumes Ethernet **connected** (today it is not, so every row below is a no-op and the UI must say so).

| Transition | Tunnel outer UDP | Tunnel drops? | App detects | Display | Warn / block |
|---|---|---|---|---|---|
| 1 → 2 (Ethernet → Wi‑Fi, metric 9000) | **Stays on Ethernet** if the `/32` is pinned there | No. No warning from Proton, no reconnect — the path never broke | `carrier != selected_link && best_cidr > 0` | STR‑PIN | **Warn, non-blocking banner.** Never block: non-tunnel and split-tunnel traffic *does* follow the metric. |
| 2 → 1 (Wi‑Fi → Ethernet, restore automatic) | Unchanged (already there, or still pinned to Wi‑Fi) | No | same | STR‑PIN if mismatch | Warn only |
| any → 3 (Wi‑Fi only, `ms_tcpip` unbind) | **Moves to Wi‑Fi.** Unbinding removes the Ethernet IPv4 interface and all its routes incl. the `/32` → `NotifyRouteChange2` fires → WireGuardNT bumps `RoutingGeneration` and re-resolves; Proton's `OnRouteChanged` re-runs `CreateServerRoute`. Ethernet also fails the `IfOperStatusUp` test under the device-disable variant. | Brief interruption (sub-second expected; **unmeasured** — Proton's 5–30 s clamp is the *initial-connect* timeout, not the re-pin latency) | route-change watcher + carrier recompute | STR‑MODE3‑OK / STR‑MODE3‑SLOW | Warn + preflight gates (§4.5). Block only on gate failure. |
| 3 → 1/2 (rebind) | Re-pins per Proton's metric-blind order once Ethernet has a gateway again | No | binding + route watchers | STR‑REBIND | No warn |
| Proton reconnect (user action, any mode) | Re-pins via `BestInterface`; **this is the only reliable way to move the tunnel between links** | Yes (user-initiated) | pin change | STR‑PIN clears | — |

**Absolute prohibition:** LinkSwitch **never deletes, re-adds or rewrites the endpoint `/32`.** Proton's `RouteChangeMonitor` (`NotifyRouteChange2`, `AF_UNSPEC`) fires on the delete and recreates it via the same metric-blind path — zero gain — and `RoutingTableHelper.DeleteRoute(ip, isIpv6)` deletes **every row matching that destination on every interface**, so racing it can transiently blackhole a live tunnel. Read it, display it, leave it alone.

### 4.5 Mode‑3 preflight gates (elevated worker, all local — no gate requires reachability)

| # | Gate | Failure |
|---|---|---|
| 1 | Wi‑Fi is `IfOperStatusUp` **and** media-connected | **Block** |
| 2 | Wi‑Fi has a non‑APIPA IPv4 unicast address **and** a `0.0.0.0/0` with a real NextHop | **Block** |
| 3 | That NextHop appears in `GetIpNetTable2` as `Reachable` or `Stale` | **Block** if absent; must be strictly **`Reachable`** when a kill switch is detected |
| 4 | Wi‑Fi's `/0` can cover the endpoint (i.e. Ethernet is not the only path to it) | **Block** |
| 5 | `ms_implat` or `vms_pp` is `True` on Ethernet (NIC team member / external Hyper‑V switch uplink) | **Block the device-disable variant outright** (both are `False` here) |
| 6 | `DisableDefaultRoutes == true` on Ethernet | Modes 1 blocked (see §6); mode 3 allowed with a note |

### 4.6 Exact user-facing strings

```
STR-PIN            (amber banner, dismissible, mode 1<->2 only)
  VPN active — this switch does not move the tunnel.
  Proton VPN's encrypted traffic is pinned to {carrier_name}. LinkSwitch changed which link
  Windows prefers, and traffic outside the tunnel follows that change immediately. The tunnel
  itself will keep using {carrier_name} until you reconnect Proton VPN or switch to "Wi-Fi only".
  [ Show me why ]   [ Dismiss ]

STR-PIN-DETAIL     (expander behind "Show me why")
  Proton VPN adds a host route to its server ({endpoint_ip}/32 via {next_hop}) and pins it to a
  network adapter chosen without reference to interface metrics. WireGuard then matches that
  longer, more specific route ahead of any default route, whatever its metric. LinkSwitch will
  not delete or edit that route — doing so causes Proton to immediately recreate it and can
  briefly break your connection.

STR-MODE3-OK       (green, transient, 5 s)
  Ethernet silenced. The VPN tunnel moved to Wi-Fi.

STR-MODE3-SLOW     (amber, shown if the tunnel has not re-pinned after 5 s)
  Ethernet is silenced, but the VPN tunnel has not settled on Wi-Fi yet.
  Give it a few seconds. If it does not recover, click Undo below, or open Proton VPN and reconnect.
  [ Undo — restore Ethernet ]

STR-KILLSWITCH     (amber, before mode 3, requires acknowledgement)
  Proton VPN's kill switch is on.
  If the tunnel cannot re-establish over Wi-Fi, Windows will have no internet at all until the
  VPN reconnects — that is what the kill switch is for. LinkSwitch will automatically restore
  Ethernet after 20 seconds if you do not confirm that the switch worked.
  Emergency escape hatch (run as administrator):
      "C:\Program Files\Proton\VPN\v5.1.7\ProtonVPN.RestoreInternet.exe"
  [ Continue ]   [ Cancel ]

STR-LAN-BLOCKED    (amber, replaces the mode-2 LAN promise when WFP blocks private ranges)
  Your VPN is blocking local network traffic.
  Ethernet will stay connected, but this PC cannot reach devices on your LAN in any LinkSwitch
  mode — that block is enforced by the VPN's firewall rules, below the routing table, and no
  LinkSwitch setting can override it. Turn on "Allow LAN connections" in Proton VPN if you need
  local access.

STR-DNS-WINDOW     (informational, during a mode switch while an NRPT rule is live)
  Name lookups may pause for a moment while the link changes. This is the VPN's DNS rule,
  not a Wi-Fi problem.

STR-AUTOREVERT     (red, on timed or boot-triggered revert)
  LinkSwitch restored Ethernet automatically.
  "Wi-Fi only" was applied but never confirmed{, because the PC restarted first}. Your adapter
  settings are back to how LinkSwitch found them.

STR-REBIND         (transient)
  Ethernet re-enabled. Waiting for Windows to bring its IP stack back…
```

---

## 5. README rescue script (VPN-safe)

```powershell
#Requires -RunAsAdministrator
<#
  LinkSwitch-Rescue.ps1  --  undo everything LinkSwitch can do, and nothing else.
  Run from an elevated PowerShell. Safe to run even if LinkSwitch was never installed.

  ############################################################################
  #  WHY THIS SCRIPT DOES *NOT* RE-ENABLE TCP/IPv6 (ms_tcpip6)
  #
  #  On many machines IPv6 is unbound ON PURPOSE. VPN clients implement "IPv6
  #  leak protection", and one common way to do that is to unbind "Internet
  #  Protocol Version 6 (TCP/IPv6)" from the physical adapters. If this script
  #  blanket-enabled every binding it would silently switch that protection off
  #  and could expose your real IPv6 address outside the tunnel -- the exact
  #  thing the VPN was preventing.
  #
  #  LinkSwitch itself only ever re-enables a binding that LinkSwitch itself
  #  disabled, using its journal at %ProgramData%\LinkSwitch\journal.json.
  #  This script is the blunt fallback for when that journal is gone, so it
  #  deliberately stays on the safe side of the line.
  #
  #  If you genuinely want IPv6 back, do it yourself, deliberately, per adapter:
  #      Enable-NetAdapterBinding -Name 'Ethernet' -ComponentID ms_tcpip6
  ############################################################################
#>
[CmdletBinding()]
param([string[]]$Name)

# NOTE: deliberately NOT $ErrorActionPreference = 'Stop'.
# Get-NetIPInterface writes a NON-terminating ObjectNotFound error when a family
# is unbound (FullyQualifiedErrorId CmdletizationQuery_NotFound). Under 'Stop'
# that becomes terminating and can abort this script AFTER re-enabling a binding
# but BEFORE restoring metrics. Each phase gets its own try/catch instead.
$ErrorActionPreference = 'Continue'

function Step($label, [scriptblock]$body) {
    try { & $body }
    catch { Write-Warning "  $label failed: $($_.Exception.Message)" }
}

if (-not $Name) {
    $Name = (Get-NetAdapter -Physical -ErrorAction SilentlyContinue |
             Where-Object { $_.InterfaceDescription -notmatch 'VMware|Hyper-V|Virtual|TAP|WireGuard|Wintun|Loopback' }
            ).Name
}
Write-Host "LinkSwitch rescue -- in scope: $($Name -join ', ')" -ForegroundColor Cyan
Write-Host "Never touched: VPN tunnels, VMnet*, vEthernet, Bluetooth, and ms_tcpip6 anywhere.`n"

foreach ($n in $Name) {
    $ad = Get-NetAdapter -Name $n -ErrorAction SilentlyContinue
    if (-not $ad) { Write-Warning "[$n] not found - skipping"; continue }

    # 1. Device -- undo the optional "disable the NIC" flavour of Wi-Fi only.
    if ($ad.Status -eq 'Disabled') {
        Step "[$n] enable device" {
            Write-Host "  [$n] device is disabled -> enabling"
            Enable-NetAdapter -Name $n -Confirm:$false
            Start-Sleep -Seconds 3
        }
        $ad = Get-NetAdapter -Name $n -ErrorAction SilentlyContinue   # ifIndex may have changed
        if (-not $ad) { continue }
    }

    # 2. IPv4 binding -- the ONLY binding this script will ever turn back on.
    #    Safe because no VPN leak-protection feature disables IPv4; if TCP/IPv4 is
    #    off on a physical NIC, LinkSwitch is the only plausible cause and the
    #    machine is half-broken until it comes back.
    Step "[$n] rebind ms_tcpip" {
        $v4 = Get-NetAdapterBinding -Name $n -ComponentID ms_tcpip -ErrorAction SilentlyContinue
        if ($v4 -and -not $v4.Enabled) {
            Write-Host "  [$n] re-enabling Internet Protocol Version 4 (TCP/IPv4)" -ForegroundColor Green
            Enable-NetAdapterBinding -Name $n -ComponentID ms_tcpip
            Start-Sleep -Seconds 5     # the adapter restarts; the IP interface takes a moment
        } else { Write-Host "  [$n] TCP/IPv4 already bound" }
    }

    # 3. IPv6 binding -- REPORT ONLY. Read, never written. See header.
    Step "[$n] report ms_tcpip6" {
        $v6 = Get-NetAdapterBinding -Name $n -ComponentID ms_tcpip6 -ErrorAction SilentlyContinue
        if ($v6 -and -not $v6.Enabled) {
            Write-Host "  [$n] TCP/IPv6 is UNBOUND - left alone on purpose (VPN leak protection is the usual cause)" -ForegroundColor Yellow
        }
    }

    # 4. Metrics -> automatic, per family, only for families that exist.
    foreach ($af in 'IPv4','IPv6') {
        Step "[$n/$af] metric" {
            # -ErrorAction SilentlyContinue is REQUIRED: with the family unbound this
            # cmdlet emits ObjectNotFound and returns nothing. Underneath,
            # GetIpInterfaceEntry returns ERROR_NOT_FOUND (1168). That is normal.
            $ipif = Get-NetIPInterface -InterfaceIndex $ad.ifIndex -AddressFamily $af -ErrorAction SilentlyContinue
            if (-not $ipif) { Write-Host "  [$n/$af] no IP interface (protocol not bound) - nothing to restore"; return }
            if ($ipif.AutomaticMetric -eq 'Disabled') {
                Write-Host "  [$n/$af] manual metric $($ipif.InterfaceMetric) -> restoring AutomaticMetric" -ForegroundColor Green
                Set-NetIPInterface -InterfaceIndex $ad.ifIndex -AddressFamily $af -AutomaticMetric Enabled
            } else { Write-Host "  [$n/$af] metric already automatic ($($ipif.InterfaceMetric))" }
        }
    }

    # 5. Report-only: split-tunnel guard. NEVER written by this script or by LinkSwitch.
    Step "[$n] report DisableDefaultRoutes" {
        $ipif = Get-NetIPInterface -InterfaceIndex $ad.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue
        if ($ipif -and $ipif.Dhcp -ne $null -and (Get-NetIPInterface -InterfaceIndex $ad.ifIndex -AddressFamily IPv4).AdvertiseDefaultRoute -ne $null) {
            # surfaced via the API in the app; here we just note the adapter was inspected
        }
    }
}

# 6. Remove BOTH scheduled tasks (on-demand worker + boot-time revert checker).
foreach ($tn in 'LinkSwitchElevatedWorker','LinkSwitchBootRevert') {
    Step "remove task $tn" {
        if (Get-ScheduledTask -TaskName $tn -ErrorAction SilentlyContinue) {
            Write-Host "`nRemoving scheduled task '$tn'"
            Unregister-ScheduledTask -TaskName $tn -Confirm:$false
        }
    }
}

# 7. Clear LinkSwitch's own state so it cannot try to "restore" anything later.
Step "clear state" {
    $sd = Join-Path $env:ProgramData 'LinkSwitch'
    if (Test-Path $sd) {
        Write-Host "Archiving LinkSwitch state to $sd\rescued-$(Get-Date -f yyyyMMdd-HHmmss)"
        Rename-Item $sd "LinkSwitch.rescued-$(Get-Date -f yyyyMMdd-HHmmss)"
    }
}

Write-Host "`n=== Final state ===" -ForegroundColor Cyan
Get-NetIPInterface -AddressFamily IPv4 -ErrorAction SilentlyContinue | Sort-Object InterfaceMetric |
    Format-Table ifIndex, InterfaceAlias, InterfaceMetric, AutomaticMetric, ConnectionState -AutoSize
Get-NetAdapterBinding -ComponentID ms_tcpip, ms_tcpip6 -ErrorAction SilentlyContinue |
    Format-Table Name, ComponentID, Enabled -AutoSize
Write-Host "If TCP/IPv6 shows False above, that is expected on a VPN machine and was NOT changed by this script."
Write-Host ""
Write-Host "Still no internet? That is your VPN's kill switch, not LinkSwitch. Run, as administrator:" -ForegroundColor Yellow
Write-Host '    & "C:\Program Files\Proton\VPN\v5.1.7\ProtonVPN.RestoreInternet.exe"'
```

---

## 6. What the app must warn about — and the exact wording

### 6.1 Rules for *when* to warn (the dives got this wrong; do not repeat it)

**Never infer configuration from a binding.** Measured across every adapter including hidden ones: `vmware_bridge`, `ms_l2bridge` and `ms_l1vhlwf` are `True` on **ProtonVPN, Bluetooth PAN, vEthernet, Ethernet (Kernel Debugger), both WAN miniports, Wi‑Fi and Ethernet** — and `False` only on VMware's own VMnet1/VMnet8. There is **no bridge or ICS adapter on this machine at all.** A warning keyed on `vmware_bridge = True` fires on every machine with VMware installed, and a warning keyed on `ms_l2bridge` claims ICS is in use on every Windows 11 box. That is warning fatigue on the exact dialog meant to protect the user.

| Concern | ✅ Correct signal | ❌ Do **not** use |
|---|---|---|
| VMware bridging | `HKLM\SYSTEM\CurrentControlSet\Services\VMnetBridge\Parameters` and VMnetLib `VMnetConfig\vmnet0\HostInterface`. **Empty here = VMware's "automatic" bridging.** Warn *assertively* only when VMnet0 is explicitly bound to the target adapter; otherwise use the hedged "may auto-bridge" wording. | `vmware_bridge` binding |
| ICS / Mobile hotspot | a Bridge/ICS adapter in `Get-NetAdapter -IncludeHidden` + `SharedAccess` service state | `ms_l2bridge` binding |
| NIC team / external vSwitch | `ms_implat` / `vms_pp` — **these two are genuinely discriminating** (both `False` here) | — |

### 6.2 Strings

```
STR-VM-AUTO        (informational, mode 3, when VMnet0 is "automatic")
  VMware is installed and may be bridging virtual machines onto Ethernet.
  Silencing Ethernet removes THIS PC's IP address on that adapter. Bridged VMs have their own
  addresses and normally keep working, because VMware's bridge sits below IP. If a VM loses
  its network, switch back to Ethernet or Wi-Fi mode.

STR-VM-PINNED      (amber, mode 3, when VMnet0 names this adapter)
  VMware bridged networking is bound to {adapter}.
  Silencing this adapter removes only this PC's IP address; bridged VMs keep their own and
  should stay connected. Choosing the "disable the adapter" option instead WILL cut them off.

STR-DEVDISABLE     (blocking confirm, device-disable variant only)
  Disabling the Ethernet device is a bigger hammer.
  It will cut off any VMware-bridged virtual machines on this adapter, and Windows will give
  the adapter a new interface number when it comes back. The default option (removing TCP/IP)
  achieves the same result for this PC without either side effect.
  [ Use the default instead ]   [ Disable the device anyway ]

STR-TEAM-BLOCK     (hard block, no override)
  Ethernet is part of a NIC team or is the uplink for a Hyper-V external switch.
  Disabling this adapter would take down the team or the virtual switch, not just this
  connection. LinkSwitch will not do that. Use "Wi-Fi" mode instead — it demotes Ethernet
  without removing it.

STR-SAMESUBNET     (amber, replaces the mode-2 subtitle when the two links share a prefix)
  Ethernet and Wi-Fi are on the same network ({prefix}).
  In Wi-Fi mode, LAN devices — your NAS, printer, or router page — will be reached over Wi-Fi
  too, not over the cable. "Ethernet keeps serving its own LAN" only applies when the two
  adapters are on different networks. Nothing LinkSwitch can set changes this.

STR-EXISTING-CONN  (informational, shown on every mode 1<->2 switch)
  Existing connections stay where they are.
  Windows keeps open connections on the link they started on. Downloads, video calls, SSH
  sessions and the VPN tunnel keep using the previous adapter until they finish or reconnect.
  New connections use {new_link} immediately.

STR-MODE3-SCOPE    (replaces any "zero networking" copy — the honest version)
  "Wi-Fi only" removes this PC's IP stack from Ethernet: no address, no routes, no traffic
  from Windows. The adapter stays powered and linked, so it still answers low-level network
  discovery, and any VMware-bridged VM keeps sending its own traffic over the cable. If you
  need the adapter to contribute literally nothing, use the "disable the adapter" option.

STR-CONFLICT       (persistent card, per abandoned entry)
  Something else changed this setting.
  {adapter} — {setting}: LinkSwitch set it to {expected}; it is now {found}. LinkSwitch found
  it as {prior} before making its change. It will not overwrite another program's change on
  its own.
  [ Restore it to {prior} ]   [ Leave it as it is ]

STR-RATCHET        (persistent card)
  Not restoring this — it would weaken a protection.
  {adapter} — {setting} is currently {found}. Putting it back to {prior} would re-enable
  something another program has switched off, most likely your VPN's leak protection.
  LinkSwitch will not do that automatically.
  [ I understand — restore it anyway ]   [ Leave it ]

STR-LOCK           (transient error)
  Windows network settings are locked by another program.
  {holder} is changing network configuration right now — most likely your VPN reconnecting.
  Nothing was changed. Try again in a few seconds.

STR-P0-DDR         (red, aborts the transition)
  Aborted: a VPN setting changed while LinkSwitch was working.
  Split-tunnel protection on {adapter} changed mid-operation. LinkSwitch stopped rather than
  risk overwriting it. Nothing was left half-applied. Try again once your VPN has settled.

STR-DDR-BLOCK      (blocks mode 1 only)
  Ethernet cannot take the default route right now.
  A VPN or security program has set "no default routes" on this adapter, so Windows will not
  send internet traffic over it whatever priority LinkSwitch sets. This is not something
  LinkSwitch will change. Wi-Fi and Wi-Fi-only modes still work normally.

STR-CABLE-OUT      (state, not warning — the app's resting state today)
  Ethernet cable is not connected. There is nothing to switch between right now.
  Wi-Fi is carrying all traffic.

STR-SMARTSCREEN    (README, not UI)
  Windows may show "Windows protected your PC" the first time you run a downloaded LinkSwitch
  build. LinkSwitch is unsigned open-source software; the warning appears because the file was
  downloaded, not because anything is wrong with it. Click More info -> Run anyway. Building
  from source with `cargo build --release` produces a binary with no such warning.
  There is no way to request a SmartScreen review for consumer PCs — reputation builds
  automatically as more people download a release.
  Separately: Microsoft Defender occasionally flags unsigned Rust binaries as
  "Trojan:Win32/Wacatac.B!ml". The "!ml" suffix means a machine-learning guess, not a match.
  Report a false positive at https://www.microsoft.com/en-us/wdsi/filesubmission
```

---

## 7. Risk register (this environment, prioritised)

| # | Sev | Risk | Why it's real *here* | Mitigation (implement, don't note) |
|---|---|---|---|---|
| R1 | **P0** | Mode 3 + persistent kill switch + crash/reboot ⇒ user boots into a machine with no internet and no obvious fix | Proton's `BlockAllIpv4Network` filters are `persistent: true`; `PermitServerAddress` and the tunnel-interface permit are **`persistent: false`**. Binding changes are registry-persistent. | `pending_revert.json` fsync'd **before** `INetCfg::Apply`; 20 s armed revert; **boot-triggered** scheduled task that re-binds on a stale marker; STR‑KILLSWITCH acknowledgement; `ProtonVPN.RestoreInternet.exe` named in UI **and** README |
| R2 | **P0** | Classifier drops the Wi‑Fi NIC ⇒ modes 2 and 3 unreachable | The dive's `MediaType == NdisMediumNative802_11` test was compiled and run: **1 match, Ethernet only**. `MIB_IF_ROW2.MediaType` for the real radio is `0`. | §1.2 classifier (no `MediaType` test); startup assertion that a `WiFi` link exists; unit test with a captured 53-row table fixture |
| R3 | **P0** | We clobber a VPN's `DisableDefaultRoutes` and silently disable split-tunnel protection | It is the documented VPN security member, is **not** in `SetIpInterfaceEntry`'s ignored-on-input list, and is rewritten by any get-modify-set. `InitializeIpInterfaceEntry` + partial fill would write `FALSE` outright. | Read-modify-write only, **never** `InitializeIpInterfaceEntry`; journal `disable_default_routes` at capture; re-read after every `Set` and abort with STR‑P0‑DDR on any change (currently `0` on ifIndex 4/14/43) |
| R4 | **P0** | LinkSwitch re-enables `ms_tcpip6` and defeats the machine's IPv6 leak posture | `ms_tcpip6` is `False` on all six physical/virtual adapters today; attribution to Proton is **unproven** (their docs never say they unbind it; their logs have zero hits) — so the rule must be actor-agnostic | `we_changed_it:false` for every no-op; single `enable_binding` call site under `pub(in crate::restore)`; CI grep fails the build on `Enable-NetAdapterBinding` or `enable_binding(` outside `src/restore/`; post-apply collateral diff auto-reverts a collateral `ms_tcpip6` enable as P0 |
| R5 | **P1** | Enumerating via `GetAdaptersAddresses` blinds the app to the NIC it just silenced | Proved: `AF_INET6` returns **2 of 22** adapters — exactly those with `ms_tcpip6` bound. Mode 3 does the same to `AF_INET` on Ethernet. | Enumerate via `GetIfTable2` exclusively; treat GAA absence as "unbound", never "gone"; `FreeMibTable` on every path |
| R6 | **P1** | `INetCfg` write-lock collision with Proton mid-reconnect leaves a half-applied mode | Proton takes the same machine-wide lock on every connect/disconnect | 5 s timeout, `appName = "LinkSwitch"`, surface the returned holder (STR‑LOCK), process-wide mutex, never force, ≤2 retries |
| R7 | **P1** | Touching Wi‑Fi's bindings drops the VPN and every TCP session | `Disable-NetAdapterBinding` restarts the adapter (all three MS doc examples say so) — and Wi‑Fi is the *only* live link today | Bindings are only ever written on the Ethernet role; `-AllBindings` is never used; assert `link.kind == Ethernet` in `apply_disable_binding` |
| R8 | **P1** | Mode 2's LAN promise is false — silently | Two independent causes: (a) Proton's `PermitPrivateNetwork()` early-returns unless "Allow LAN" is on, and `tunnel.dll`'s own `blockAll()` sublayer has **no LAN permit at all**; (b) Wi‑Fi is `192.168.1.0/24` and the cable almost certainly goes to the same router → raising Ethernet's metric also demotes its on-link `/24`. | Compare unicast prefixes at mode-switch time → STR‑SAMESUBNET; elevated WFP scan for `ProtonVPN permit private network` **and** the `WireGuard filters` sublayer → STR‑LAN‑BLOCKED; never ship the "keeps serving its own LAN" copy unconditionally |
| R9 | **P1** | User believes mode 2 moved the VPN; it did not | Metric-blind `BestInterface` + longest-prefix `SocketResolvePeerEndpoint`. **Never observed here** (cable is out) — extrapolated from source. | Runtime pin detection + `resolve_tunnel_carrier`; STR‑PIN banner; never hardcode "Proton pins to Ethernet"; **re-verify by physically plugging the cable in and re-running `Get-NetRoute` before shipping any copy that names a carrier** |
| R10 | **P2** | Metric change does not survive a reboot ⇒ mode 2 silently reverts; worse, a restore may fail to undo a change that *is* persistent | `SetIpInterfaceEntry` has no store selector while `netsh`/`Set-NetIPInterface` do; **no `InterfaceMetric` REG_DWORD exists anywhere** under either TCP/IP hive even though vEthernet reads 5000 and ProtonVPN reads 0 with `AutomaticMetric Disabled` | **Verify on a throwaway VM before shipping mode 2.** Boot task re-asserts the journalled mode's metric **only when** `phase == applied && live == prior` (a reboot revert), logging each re-assert. If the API proves non-persistent for restore too, switch both apply and restore to `Set-NetIPInterface` semantics — never mix. |
| R11 | **P2** | Identity resolution binds a role to an NDIS filter module | 22 LWF rows carry the **same** `PermanentPhysicalAddress` as their host NIC (`AABBCCDDEE01` on idx 4/25/26/27) | Resolve by MAC **only over `classify()`-accepted rows**; `F_FILTER_INTERFACE` reject in the gate |
| R12 | **P2** | Phantom "Ethernet" devices get picked as the target | idx 2/13/21 (Apple Mobile Device, UsbNcm ×2) pass the entire flag gate; only `OperStatus == NotPresent` rejects them, and the widened `PhysicalMediumUnspecified` arm would otherwise admit them | The `OperStatus` gate is mandatory and covered by a regression test |
| R13 | **P2** | `ifIndex` reuse after a device-disable binds an operation to the wrong adapter | Documented non-persistent; the device-disable variant is precisely that event | Persist MAC only; re-resolve `ifIndex`/`luid` at the start of **every** operation; never cache across an await/sleep |
| R14 | **P2** | Hidden adapters outrank Wi‑Fi in any naive "lowest metric wins" display | idx 16/17 (Wi‑Fi Direct) sit at IPv4 metric **25**, below Wi‑Fi's 30, and are invisible to `Get-NetAdapter` | Rank only interfaces that actually own a `0.0.0.0/0` route; display from `MIB_IPINTERFACE_ROW.Metric`, never from PowerShell/CIM (both `Get-NetIPInterface` and `Get-NetRoute` return a **blank** InterfaceMetric for ifIndex 43 — the one row that matters) |
| R15 | **P2** | DNS blackout during a mode switch is misread as "Wi‑Fi has no internet" | A global NRPT rule `"." → 10.2.0.1` is live, kept by `ProtonVPN.NrptWatchdog.exe`; if the tunnel takes a second to re-pin, *all* resolution fails, LAN names included | LinkSwitch performs **zero DNS** during a mode switch (no update check, no telemetry); STR‑DNS‑WINDOW; the watchdog's "black hole" verdict requires all three conditions in §4.3 |
| R16 | **P3** | Second tunnel confuses VPN logic | `Tailscale` (Wintun) is ifIndex 9, `IfType 53`, currently Down, absent from `Get-NetAdapter` but present in `GetIfTable2`; `Win32_NetworkAdapter` calls it a physical adapter with an empty PNPDeviceID | Tunnel detection is *behavioural* (Up + not steerable-physical + owns the winning `/0`), never "the one `IfType 53` adapter"; the UI lists all detected tunnels |
| R17 | **P3** | Worker runs unelevated and every `Set` fails with 5 | The GUI manifest must stay `asInvoker` (one binary, no UAC per click), so `requestedExecutionLevel` cannot be used to guarantee the worker's token | Worker checks its own `TokenElevation` via `GetTokenInformation` at start and exits with a distinct "worker not elevated — re-register the scheduled task" code; the GUI surfaces that specific message, never a generic failure |
| R18 | **P3** | SmartScreen / Defender friction on release downloads | SAC is **Off** here and RTP + Tamper Protection are On, so the developer never sees the worst case a clean Win11 user hits (SAC is a hard block with no "Run anyway") | STR‑SMARTSCREEN in the README, with the SmartScreen and Wacatac paragraphs kept **separate** — WDSI submission is for the Defender false positive, *not* a SmartScreen remedy |
| R19 | **P3** | `Get-NetAdapterBinding` returns no row for some adapters, and the pre/post diff panics | 22 adapters exist but only 10 have binding rows | Snapshot diff tolerates a missing component and a missing adapter row; a missing row is `None`, never `false` |
| R20 | **P3** | Corporate EDR flags the `RunLevel HIGHEST` scheduled task as persistence (MITRE T1053.005) | Defender did not object here, but this is a plausible blocker elsewhere | README note; task named `LinkSwitchElevatedWorker` with a descriptive `Description` field; no obfuscation, no `-EncodedCommand` |

### Implementation gates before v0.1

1. **Compile-and-run** the §1.2 classifier against the live table; assert exactly `{4: Ethernet, 14: WiFi}`.
2. **VM-only:** verify `SetIpInterfaceEntry` metric persistence across reboot (R10). This gates mode 2.
3. **VM-only:** verify that unbinding `ms_tcpip` yields `AF_INET → 1168` (strongly indicated: seven adapters already return `1168` for `AF_INET` today) and that the tunnel re-pins.
4. **Cable in, read-only:** re-run `Get-NetRoute` with Ethernet connected to confirm the endpoint-pin direction before shipping any copy that names a carrier (R9).
5. **CI:** grep gate for `Enable-NetAdapterBinding` / `enable_binding(` outside `src/restore/` (R4); `#![deny(non_upper_case_globals)]` in the classifier module — an out-of-scope or misspelled `NdisPhysicalMedium*` in a `match` pattern silently becomes a fresh binding that matches **everything**.