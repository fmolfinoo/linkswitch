# LinkSwitch — Implementation Blueprint

**Target:** `C:\dev\linkswitch`, MIT, Rust, Windows 10/11 x86_64-msvc, stable toolchain (verified on rustc 1.96.1).

---

## 1. VERDICT ON FEASIBILITY

### 1.1 Does interface-metric steering deliver the goal?

**Yes, for routing. Unconditionally.** Windows selects a route by longest prefix match, then by *lowest total metric = route metric + interface metric* (first-party, `MIB_IPINTERFACE_ROW.Metric` docs, verbatim: "the actual route metric used to compute the route preference is the summation of the route metric offset … and the interface metric"). DHCP-installed default routes carry `RouteMetric = 0` on this machine, so the interface metric alone decides.

Raising Ethernet's IPv4 **and** IPv6 interface metrics above Wi-Fi's causes:

- new connections to egress Wi-Fi,
- Ethernet to stay **enabled, link-up, addressed, and LAN-reachable**,
- the source address to follow the chosen interface, because Vista+ defaults to the **strong host model** (`WeakHostSend`/`WeakHostReceive` = FALSE, verified live) — there is no RFC-6724 wrong-source-address leak.

Three honest limits that must appear in the UI, not be hidden:

| Limit | Why | UI copy |
|---|---|---|
| Existing TCP connections do not migrate | A socket keeps its 4-tuple and bound interface. Ethernet is still up, so nothing breaks — the transfer simply finishes on the old link. | "Applies to new connections. Transfers already running finish on their current link." |
| DNS still leaves Ethernet | Smart multi-homed name resolution issues **parallel** DNS/LLMNR/NetBT queries across *all* networks; even the "turn it off" policy still says "DNS queries will be issued across all networks first." Metric only decides which answer is preferred. | "Your connections egress Wi-Fi. DNS lookups are still emitted on both links." |
| LAN traffic behaviour depends on topology | Same-subnet (this user's case: both NICs on `192.168.1.0/24`) → both interfaces hold an equal-length on-link route, so LAN moves with the flip. Different-subnet (dock + corp LAN) → longest-prefix match pins LAN to Ethernet regardless of metric. | Show it; never promise one behaviour. |

### 1.2 Does Windows keep Wi-Fi connected when Ethernet is present?

**No. Not by default. This is the single thing that would make a naive build appear broken.**

Windows Connection Manager's *Minimize the number of simultaneous connections* policy is **ON by default** — empirically proven twice on this machine (`WcmQueryProperty(wcm_global_property_minimize_policy)` → `fValue = 1`, `fIsGroupPolicy = 0`, with **no** registry value present). An absent `fMinimizeConnections` means **enabled**, not disabled. (A naive `Get-ItemProperty -ErrorAction SilentlyContinue` probe reports "no such key" for a key that exists with zero values — do not use that shape.)

At value 1:

- "any new **automatic** internet connection is blocked when the computer has at least one active internet connection to a preferred type of network … Ethernet is always preferred when connected." → **after any boot/resume with the cable in, Wi-Fi will not auto-connect**, so the widget has no Wi-Fi link to steer to.
- Already-connected Wi-Fi is a soft-disconnect candidate: WCM samples traffic every 30 s and drops the interface once traffic falls below threshold. This produces an intermittent bug class — Wi-Fi survives while you browse, then vanishes when you walk away.
- Raising Ethernet's metric gives Wi-Fi **zero** protection: WCM's keep/drop decision uses a fixed media preference (Ethernet > WLAN > cellular) and is explicitly "not for routing" and never link speed.

**Required extra measures — both ship:**

**(A) On-demand association (primary, non-invasive, always ships).** The same MS doc states "Users can still manually connect to any network," and the keep-list includes "Any networks manually connected during the current user session." So the elevated worker **connects Wi-Fi itself** before applying metrics:

1. Query WLAN interface state (`WlanEnumInterfaces` → `isState`).
2. If not `wlan_interface_state_connected`, call `WlanConnect` with `wlan_connection_mode_auto` (Windows picks from the profile list).
3. Poll for `connected` + an IPv4 address for up to 12 s.
4. Only then apply metrics. If Wi-Fi never associates, **abort the flip and report** — never leave the user with Ethernet parked and no Wi-Fi.

**(B) Opt-in policy override (secondary, off by default).** A settings checkbox — *"Keep Wi-Fi connected while Ethernet is plugged in"* — writes `HKLM\SOFTWARE\Policies\Microsoft\Windows\WcmSvc\GroupPolicy!fMinimizeConnections = REG_DWORD 0` from the elevated installer, backing up the prior state (absent vs. value N) and restoring it verbatim on uninstall.

Hard rules for (B):
- **Write 0. Never "disable"/delete.** The ADMX enum has `required="true"` and **no `<disabledValue>`**, so deleting the value restores the OS default of 1.
- **Refuse when `fIsGroupPolicy = 1`.** Domain policy will revert your write at the next `gpupdate`.
- **Refuse when the effective value is 3.** Value 3 ("Prevent Wi-Fi when on Ethernet") blocks even *manual* WLAN connection — LinkSwitch cannot work; say so plainly instead of failing silently.
- **Warn about compliance.** DISA STIG Windows 11 **WN11-CC-000055 / V-253364 requires value 3**, and explicitly flags 0 as a finding. Say this in the README and in the installer output. This is why it is opt-in and off by default.
- Read the **registry DWORD** as the authority for the *enum* (0/1/2/3); use `WcmQueryProperty` only to obtain the *effective default* when the value is absent (`WCM_POLICY_VALUE.fValue` is a `BOOL`, so a GP-set 3 may not survive the round trip through it).

### 1.3 Overall verdict

**Feasible and worth building**, with a two-part mechanism: *metric steering for routing* + *on-demand WLAN association to defeat WCM*. Microsoft itself endorses the routing half: "If both are connected, the user or a desktop app can change route metrics to influence routing preferences."

Two decisions that follow from the research and are non-negotiable:

- The **elevated worker connects Wi-Fi**; the metric flip alone is not a product.
- The **"who is winning" readout is derived from the routing table**, never from a bare `GetBestRoute2` — on this very machine an unpinned `GetBestRoute2(8.8.8.8)` returns ifIndex 43 (ProtonVPN), not Ethernet or Wi-Fi.

---

## 2. CARGO.TOML (verbatim)

```toml
[package]
name        = "linkswitch"
version     = "0.1.0"
edition     = "2021"
rust-version = "1.82"          # windows 0.62.2 MSRV
license     = "MIT"
build       = "build.rs"
description = "Switch Windows internet traffic between Ethernet and Wi-Fi without unplugging the cable."
repository  = "https://github.com/<you>/linkswitch"

[dependencies]
# eframe defaults pull wgpu: 167 unique crates / ~14 MB.
# glow: 96 unique crates / 5.9 MB release with the profile below. Both verified to build
# and launch a borderless transparent always-on-top window on this machine.
# Dropping winit/default is safe: eframe declares winit with features=["rwh_06"] directly,
# and winit's own defaults are Linux-only apart from rwh_06.
eframe = { version = "0.36.1", default-features = false, features = ["glow", "default_fonts"] }
egui   = "0.36.1"

tray-icon = "0.24.2"

raw-window-handle = "0.6.2"    # matches eframe 0.36.1 -> winit 0.30.13; Win32WindowHandle.hwnd: NonZeroIsize

serde      = { version = "1", features = ["derive"] }
serde_json = "1"

[dependencies.windows]
version = "0.62.2"
features = [
  # --- verified-minimal net core (this trio alone compiles the IP Helper layer) ---
  "Win32_Foundation",                                  # WIN32_ERROR, NO_ERROR, ERROR_* (also transitive)
  "Win32_NetworkManagement_IpHelper",                   # Get/SetIpInterfaceEntry, GetIpInterfaceTable,
                                                        # GetIpForwardTable2, GetAdaptersAddresses,
                                                        # GetIfEntry2, FreeMibTable, Notify*Change
  "Win32_NetworkManagement_Ndis",                        # REQUIRED: NET_LUID_LH, IF_OPER_STATUS,
                                                        # NDIS_PHYSICAL_MEDIUM
  "Win32_Networking_WinSock",                            # REQUIRED: ADDRESS_FAMILY, AF_INET/AF_INET6,
                                                        # SOCKADDR*, IN_ADDR/IN6_ADDR
  # --- additive, one per namespace used elsewhere ---
  "Win32_NetworkManagement_WiFi",                        # WlanOpenHandle/EnumInterfaces/QueryInterface/Connect
  "Win32_NetworkManagement_WindowsConnectionManager",    # WcmQueryProperty, WCM_POLICY_VALUE
  "Win32_Security",                                      # OpenProcessToken, GetTokenInformation, TOKEN_ELEVATION
  "Win32_System_Com",                                    # CoInitializeEx / CoCreateInstance
  "Win32_System_Console",                                # AttachConsole(ATTACH_PARENT_PROCESS)
  "Win32_System_Registry",                               # RegGetValueW / RegSetKeyValueW (WCM policy)
  "Win32_System_TaskScheduler",                          # ITaskService, ITaskFolder, IRegisteredTask
  "Win32_System_Threading",                              # GetCurrentProcess
  "Win32_System_Variant",                                # VARIANT (NOT in ::System::Com)
  "Win32_UI_Shell",                                      # ShellExecuteExW (runas self-elevation)
  "Win32_UI_WindowsAndMessaging",                         # SW_*, GetWindowLongPtrW/SetWindowLongPtrW, GetSystemMetrics
]

[build-dependencies]
embed-manifest = "1.5.0"   # AsInvoker + PerMonitorV2Only + longPathAware + UTF-8 codepage, by default
embed-resource = "3.0.11"  # embeds assets/app.ico

[profile.release]
opt-level     = "z"
lto           = true
codegen-units = 1
strip         = true
panic         = "abort"
```

**Feature-gating rule to remember:** in windows-rs 0.62.x a function is gated on the feature of **every namespace appearing in its signature**, not just its own module (`CreateFileW` needs `Win32_Security`; `WriteFile` needs `Win32_System_IO`). If you hit `error[E0432]: unresolved import`, read the compiler's `note: found an item that was configured out … gated behind the "X" feature` — it names the exact feature.

**Docs:** `docs.rs/windows` is a stub and deep links 404. Use `https://microsoft.github.io/windows-docs-rs/doc/windows/...` or grep the vendored source at `%USERPROFILE%\.cargo\registry\src\index.crates.io-*\windows-0.62.2\src\Windows\Win32\...`.

### build.rs

```rust
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Defaults: requestedExecutionLevel=asInvoker, dpiAwareness=permonitorv2 (PerMonitorV2Only),
        // supportedOS Win7..Win11, longPathAware, activeCodePage=UTF-8, Common-Controls 6.0.0.0.
        // DO NOT set requireAdministrator: this same exe runs unelevated as the widget.
        embed_manifest(new_manifest("dev.linkswitch.LinkSwitch")).expect("embed manifest");

        // Icon-only .rc — must contain no manifest directive, or it fights embed-manifest.
        embed_resource::compile("assets/app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embed icon");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
}
```

`assets/app.rc`:
```
1 ICON "app.ico"
```

---

## 3. MODULE LAYOUT

```
linkswitch/
├─ Cargo.toml
├─ build.rs
├─ LICENSE                     MIT
├─ README.md                   incl. the WCM/STIG disclosure and the DNS/existing-connection caveats
├─ assets/{app.rc, app.ico}
└─ src/
   ├─ main.rs
   ├─ cli.rs
   ├─ log.rs
   ├─ config.rs
   ├─ elevate.rs
   ├─ install.rs
   ├─ tasks.rs
   ├─ apply.rs
   ├─ net/
   │  ├─ mod.rs
   │  ├─ err.rs
   │  ├─ luid.rs
   │  ├─ adapters.rs
   │  ├─ metric.rs
   │  ├─ routes.rs
   │  ├─ notify.rs
   │  ├─ wifi.rs
   │  └─ wcm.rs
   └─ ui/
      ├─ mod.rs
      ├─ widget.rs
      ├─ tray.rs
      └─ hwnd.rs
```

| File | Responsibility | Key items |
|---|---|---|
| `main.rs` | `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`. Attach console when argv has a subcommand, dispatch, set process exit code. | `fn main() -> ExitCode`, `fn attach_console_if_cli()` |
| `cli.rs` | Hand-rolled argv parse (no clap — 4 verbs). Rejects anything unrecognised. | `enum Cmd { Gui, Apply(Mode), Restore, Install, Uninstall, SetAdapters, Status }`, `enum Mode { Ethernet, Wifi, Auto }` |
| `log.rs` | Append-only line log to `%ProgramData%\LinkSwitch\logs\linkswitch.log` (rotate at 1 MB). The worker has no console under Task Scheduler, so **file logging is the diagnostics channel**. | `fn init(role: &str)`, `macro_rules! lslog` |
| `config.rs` | Two stores. **Machine** `%ProgramData%\LinkSwitch\config.json` (admin-write DACL): adapter LUIDs, park metric, `wcm_backup`, `wifi_profile_hint`. **Machine state** `state.json`: last applied mode + per-family outcome + timestamp. **User** `%APPDATA%\LinkSwitch\prefs.json`: window x/y, hidden-to-tray flag. | `struct MachineConfig`, `struct AppliedState`, `struct UserPrefs`, `load_machine()`, `save_machine()` (elevated only), `load_prefs()/save_prefs()` |
| `elevate.rs` | `is_elevated()` via `OpenProcessToken(GetCurrentProcess()) + GetTokenInformation(TokenElevation)`. **Never** infer admin-ness from group SIDs — verified on this machine that `WindowsIdentity.Groups` omits `S-1-5-32-544` under a filtered token. `relaunch_elevated(args)` via `ShellExecuteExW` `lpVerb = "runas"`, `SEE_MASK_NOCLOSEPROCESS`, then `WaitForSingleObject` + `GetExitCodeProcess`. | `fn is_elevated() -> bool`, `fn relaunch_elevated(&[&str]) -> io::Result<u32>` |
| `install.rs` | Install: verify elevation (else self-elevate), copy exe → `%ProgramFiles%\LinkSwitch\linkswitch.exe`, create `%ProgramData%\LinkSwitch` with an explicit DACL (Administrators+SYSTEM full, Users read), write default config, register 4 tasks, optionally apply the WCM policy. Uninstall: `--apply auto` first, restore WCM backup, delete tasks + folder. | `fn install(opts)`, `fn uninstall()`, `fn ensure_dacl(path)` |
| `tasks.rs` | Task Scheduler 2.0 COM. Registration by **XML** (`ITaskFolder::RegisterTask`), so the shipped XML is literally what runs. Trigger by `IRegisteredTask::Run(&VARIANT::default())`. COM work always on a **dedicated short-lived thread** that does its own `CoInitializeEx(COINIT_APARTMENTTHREADED)`, so it never collides with winit's own COM apartment on the UI thread. | `fn register_all(exe, user)`, `fn unregister_all()`, `fn run(task: TaskId) -> Result<()>`, `fn last_result(task) -> Result<i32>`, `fn action_path(task) -> Result<String>` |
| `apply.rs` | **The elevated worker.** `fn apply(mode) -> ApplyReport`. Sequence in §5.4. Writes `state.json` and the log; exit code 0 = success. | `struct ApplyReport`, `fn apply(Mode)`, `fn restore_from_state()` |
| `net/err.rs` | WIN32_ERROR helpers. **Never** convert to `windows::core::Error` and compare against a `WIN32_ERROR` — `.ok()` mangles 5 into HRESULT 0x80070005 and every comparison silently becomes false. | `fn explain(WIN32_ERROR) -> &'static str`, `fn is_absent(WIN32_ERROR) -> bool` |
| `net/luid.rs` | `NET_LUID_LH` is a **union with no `PartialEq`/`Debug`/`Hash`** — comparing two directly is a hard compile error. Store `u64` everywhere. | `fn luid_u64(NET_LUID_LH) -> u64`, `fn luid_from_u64(u64) -> NET_LUID_LH`, `struct LuidKey(u64)` |
| `net/adapters.rs` | `GetAdaptersAddresses` (retry loop, `Vec<u64>` buffer) + `GetIfEntry2` per adapter. Produces the candidate list. | `struct Nic`, `fn enumerate() -> Vec<Nic>`, `fn candidates() -> (Vec<Nic> /*eth*/, Vec<Nic> /*wifi*/)` |
| `net/metric.rs` | The core. Per-(LUID, family) get/set/restore with the field traps. | `enum FamilyOutcome`, `fn read(luid, family)`, `fn set(luid, family, Option<u32>)`, `fn steer(luid, Option<u32>) -> SteerOutcome` |
| `net/routes.rs` | `GetIpForwardTable2` for both families; default-route (`PrefixLength == 0`) analysis with **total metric = route metric + interface metric**. | `struct DefaultRoute`, `fn default_routes(family) -> Vec<DefaultRoute>`, `fn winner(cands: &[LuidKey]) -> Verdict` |
| `net/notify.rs` | `NotifyIpInterfaceChange` + `NotifyRouteChange2` + `NotifyUnicastIpAddressChange`, all three. Callbacks set an `AtomicBool` and call a cloned `egui::Context::request_repaint()` — nothing else. | `fn register(ctx) -> Handles`, `fn unregister(Handles)` |
| `net/wifi.rs` | wlanapi status + on-demand connect. | `struct WifiStatus { ssid, quality, state }`, `fn status() -> Option<WifiStatus>`, `fn connect_auto(timeout) -> Result<()>` |
| `net/wcm.rs` | Effective minimize policy read + opt-in write/restore. | `enum MinimizePolicy { Allow, Minimize, MinimizeCellular, PreventWifi }`, `fn effective() -> (MinimizePolicy, bool /*is_gp*/)`, `fn set_allow()`, `fn restore(backup)` |
| `ui/mod.rs` | `LinkSwitchApp` + `impl eframe::App` (`logic`, `ui`, `clear_color`). Owns the live snapshot, the tray receiver, the pending-apply state machine. | `struct LinkSwitchApp`, `struct Snapshot` |
| `ui/widget.rs` | All drawing. | `fn draw(&mut self, ui: &mut egui::Ui)` |
| `ui/tray.rs` | Tray icon construction + **push-based** event handler. | `fn build(ctx: egui::Context) -> TrayIcon` |
| `ui/hwnd.rs` | One-shot HWND tweaks (WS_EX_TOOLWINDOW to also leave Alt-Tab). | `fn hwnd_of(&eframe::Frame) -> Option<HWND>`, `fn hide_from_alt_tab(HWND)` |

---

## 4. CORE NET LOGIC

### 4.1 Error handling (`net/err.rs`)

`GetIpInterfaceEntry`, `SetIpInterfaceEntry`, `GetIpInterfaceTable`, `GetIpForwardTable2`, `GetIfEntry2`, `GetBestRoute2`, `Notify*Change`, `CancelMibChangeNotify2` all return **`WIN32_ERROR`** (a `u32` newtype), *not* `windows::core::Result`. `GetAdaptersAddresses` returns a **bare `u32`**. `WcmQueryProperty` returns a bare `u32`.

```rust
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BUFFER_OVERFLOW, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
    ERROR_NOT_FOUND, NO_ERROR, WIN32_ERROR,
};

/// The interface exists but this address family is not bound to it (IPv6 unchecked).
/// This is NORMAL — 8 of 10 interfaces on the dev machine return it for AF_INET6.
#[inline]
pub fn is_family_absent(rc: WIN32_ERROR) -> bool { rc == ERROR_NOT_FOUND }

/// The LUID/index does not exist at all — the saved adapter is gone.
#[inline]
pub fn is_adapter_gone(rc: WIN32_ERROR) -> bool { rc == ERROR_FILE_NOT_FOUND }

pub fn explain(rc: WIN32_ERROR) -> &'static str {
    match rc {
        NO_ERROR                => "ok",
        ERROR_ACCESS_DENIED     => "not elevated — the scheduled task did not supply an elevated token",
        ERROR_FILE_NOT_FOUND    => "adapter no longer exists — reconfigure LinkSwitch",
        ERROR_NOT_FOUND         => "address family not bound on this adapter — nothing to do",
        ERROR_INVALID_PARAMETER => "bad Family, or SitePrefixLength was not reset to 0 on an IPv4 Set",
        _                       => "unexpected Win32 error",
    }
}
// NOTE: WIN32_ERROR consts ARE usable as match patterns (verified). But NEVER do
//   WIN32_ERROR(e.code().0 as u32) == ERROR_ACCESS_DENIED
// after .ok()? — that compares 0x80070005 against 5 and is always false.
```

### 4.2 Adapter enumeration and candidate selection (`net/adapters.rs`)

```rust
pub struct Nic {
    pub luid: u64,              // NET_LUID_LH.Value — the STABLE key. Persist this, never ifIndex.
    pub if_index: u32,          // transient; ok for display and for a fast re-lookup
    pub friendly_name: String,
    pub description: String,
    pub if_type: u32,           // 6 = Ethernet, 71 = IEEE80211
    pub kind: NicKind,          // Ethernet | Wifi | Other
    pub oper_up: bool,
    pub media_connected: bool,  // MIB_IF_ROW2.MediaConnectState == 1  <-- "is the cable in?"
    pub hardware: bool,         // InterfaceAndOperStatusFlags bit 0
    pub connector_present: bool,// bit 2
    pub filter_interface: bool, // bit 1
    pub tx_speed_bps: Option<u64>,   // None when the raw value is u64::MAX
    pub ipv4: Vec<Ipv4Addr>,
    pub ipv4_metric: u32,       // IP_ADAPTER_ADDRESSES_LH.Ipv4Metric — free, agrees with the API
}
```

**Buffer + retry (Microsoft's own algorithm, with the alignment fix):**

```rust
const MAX_TRIES: u32 = 3;
let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST
          | GAA_FLAG_SKIP_MULTICAST   | GAA_FLAG_SKIP_DNS_SERVER;
let family = AF_UNSPEC.0 as u32;            // bare u32 here; GetIpInterfaceTable takes typed ADDRESS_FAMILY
let mut size: u32 = 15_000;                 // MS sample's starting size
let mut tries = 0u32;
let (buf, head) = loop {
    // Vec<u64>, NOT Vec<u8>: IP_ADAPTER_ADDRESSES_LH has align_of == 8 (size 448) and
    // Vec<u8> guarantees only align 1. Casting a u8 buffer is UB and Miri flags it.
    let mut buf: Vec<u64> = vec![0u64; (size as usize).div_ceil(8) + 1];
    let head = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
    let rc = unsafe { GetAdaptersAddresses(family, flags, None, Some(head), &mut size) };
    tries += 1;
    if rc == 0 { break (buf, head as *const IP_ADAPTER_ADDRESSES_LH); }
    if rc != ERROR_BUFFER_OVERFLOW.0 || tries >= MAX_TRIES { return Vec::new(); }
    // loop: adapters can appear between calls (VPN, Wi-Fi Direct, VMware) — one extra
    // pass is not enough, and a single retry is exactly the bug that only shows up on
    // a user's machine.
};
// Walk with `cur = (*cur).Next`. IfIndex lives in a nested anonymous union:
let if_index = unsafe { a.Anonymous1.Anonymous.IfIndex };
// PWSTR::to_string is unsafe and must be null-checked first.
```

**Hardware discrimination — use `GetIfEntry2`, not name matching:**

```rust
const HARDWARE_INTERFACE: u8 = 0x01;  // bit 0
const FILTER_INTERFACE:   u8 = 0x02;  // bit 1
const CONNECTOR_PRESENT:  u8 = 0x04;  // bit 2

let mut row = MIB_IF_ROW2::default();
row.InterfaceLuid = luid_from_u64(luid);
if unsafe { GetIfEntry2(&mut row) } != NO_ERROR { return None; }

// windows-rs collapses the C bitfield to an opaque byte with NO accessors:
//   pub struct MIB_IF_ROW2_0 { pub _bitfield: u8 }
let f = row.InterfaceAndOperStatusFlags._bitfield;
let physical = (f & HARDWARE_INTERFACE) != 0
            && (f & CONNECTOR_PRESENT)  != 0
            && (f & FILTER_INTERFACE)   == 0;

let media_connected = row.MediaConnectState == MediaConnectStateConnected; // plain enum, 1
```

On the dev machine (22 adapters with `-IncludeHidden`) this selects **exactly two**: ifIndex 4 (Intel I225-V) and ifIndex 14 (RZ608 Wi-Fi). Every distractor is rejected — both "Microsoft Wi-Fi Direct Virtual Adapter" entries (which report `IfType = 71`!), VMware VMnet1/8, Hyper-V vEthernet, Bluetooth PAN, ProtonVPN.

**Classification (locale-independent — never substring-match `Description`, it is localized):**

```rust
use windows::Win32::NetworkManagement::Ndis::{
    NdisPhysicalMedium802_3,          // = 14  (Ethernet)   <-- NOT 1
    NdisPhysicalMediumNative802_11,   // = 9   (Wi-Fi)
    NdisPhysicalMediumWirelessLan,    // = 1   (Wi-Fi)      <-- 1 is WIRELESS, a classic mix-up
    NdisPhysicalMediumWirelessWan,    // = 8
};
let kind = match row.PhysicalMediumType {
    NdisPhysicalMedium802_3 => NicKind::Ethernet,
    NdisPhysicalMediumNative802_11 | NdisPhysicalMediumWirelessLan | NdisPhysicalMediumWirelessWan
                            => NicKind::Wifi,
    _ => match if_type { 6 => NicKind::Ethernet, 71 => NicKind::Wifi, _ => NicKind::Other },
};
```

**Rejected as useless:** `NET_IF_CONNECTION_TYPE` — measured `Dedicated (1)` for all 10 adapters including software loopback and the WireGuard tunnel. Zero discriminating power. Do not add the syscall.

### 4.3 Read / set / restore metric (`net/metric.rs`) — the field traps

```rust
use windows::Win32::Foundation::{NO_ERROR, WIN32_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetIpInterfaceEntry, SetIpInterfaceEntry, MIB_IPINTERFACE_ROW,
};
use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows::Win32::Networking::WinSock::{ADDRESS_FAMILY, AF_INET, AF_INET6};

#[derive(Debug, Clone, Copy)]
pub enum FamilyOutcome {
    /// Metric changed (or already correct).
    Applied { before: u32, before_auto: bool, after: u32, after_auto: bool },
    /// ERROR_NOT_FOUND (1168): the family is not bound on this adapter. SUCCESS, not failure.
    FamilyAbsent,
    Failed(WIN32_ERROR),
}

/// `target = Some(m)` pins a manual metric; `None` restores Windows' automatic metric.
/// Caller MUST be elevated: Set returns ERROR_ACCESS_DENIED (5) otherwise, and
/// Administrators-group membership under a filtered token is NOT sufficient.
pub fn set(luid: NET_LUID_LH, family: ADDRESS_FAMILY, target: Option<u32>) -> FamilyOutcome {
    // TRAP 1: build a FRESH zeroed row per (luid, family). MIB_IPINTERFACE_ROW is 168 bytes
    // and derives Copy — reusing one variable for the v4 then v6 pass silently sends a stale Family.
    // TRAP 2: do NOT call InitializeIpInterfaceEntry. It fills 132 of 168 bytes with 0xFF
    // sentinels, including offsets 40/44/156 which windows-rs types as Rust `bool`.
    // A `bool` holding 0xFF is an invalid bit pattern — merely READING UseAutomaticMetric
    // afterwards is undefined behaviour. `::default()` is a plain zeroed() and is correct.
    let mut row = MIB_IPINTERFACE_ROW::default();
    row.Family        = family;          // AF_UNSPEC here returns ERROR_INVALID_PARAMETER (87)
    row.InterfaceLuid = luid;            // LUID, not InterfaceIndex — index is not persistent
    // row.InterfaceIndex stays 0: a nonzero LUID takes precedence.

    let rc = unsafe { GetIpInterfaceEntry(&mut row) };
    match rc {
        NO_ERROR => {}
        r if crate::net::err::is_family_absent(r) => return FamilyOutcome::FamilyAbsent,
        r => return FamilyOutcome::Failed(r),
    }

    let (before, before_auto) = (row.Metric, row.UseAutomaticMetric);

    match target {
        Some(m) => {
            // TRAP 3: Metric is IGNORED unless UseAutomaticMetric is false.
            // TRAP 4: these are plain Rust `bool` in windows-rs 0.62.2 — NOT u8, NOT BOOL.
            //         `row.UseAutomaticMetric = 0;` is error[E0308].
            row.UseAutomaticMetric = false;
            row.Metric = m;
        }
        None => {
            // TRAP 5: you CANNOT restore automatic by writing back a remembered number.
            // Under automatic, Get reports the STACK-COMPUTED value (5/25/30/35/65/75...).
            // Writing that back pins it as a manual metric that stops tracking link speed.
            row.UseAutomaticMetric = true;
            row.Metric = 0;   // defensive: the SetIpInterfaceEntry "ignored on Set" list does
                              // NOT include Metric or UseAutomaticMetric.
        }
    }

    // TRAP 6, the big one. learn.microsoft SetIpInterfaceEntry Remarks, stated TWICE:
    // "However for IPv4, an application must not try to modify the SitePrefixLength member...
    //  For IPv4, the SitePrefixLength member must be set to 0."
    // Get hands you 64 for AF_INET on every auto-metric row on this machine (ifIndex 1,4,6,
    // 14,16,17,19,23), and MIB_IPINTERFACE_ROW docs say ">32 is an illegal value" for IPv4.
    // Passing it straight through => ERROR_INVALID_PARAMETER (87).
    // IPv4 ONLY. For AF_INET6 round-trip whatever Get returned (legal range <= 128).
    if family == AF_INET {
        row.SitePrefixLength = 0;
    }

    let rc = unsafe { SetIpInterfaceEntry(&mut row) };
    if rc == NO_ERROR {
        FamilyOutcome::Applied {
            before, before_auto,
            after: target.unwrap_or(0),
            after_auto: target.is_none(),
        }
    } else {
        FamilyOutcome::Failed(rc)
    }
}

pub struct SteerOutcome { pub v4: FamilyOutcome, pub v6: FamilyOutcome }

/// Steer BOTH families. IPv6 is best-effort: on the dev machine `ms_tcpip6` is UNBOUND on
/// Wi-Fi, Ethernet, ProtonVPN, Bluetooth and both VMware adapters, so the v6 leg returns
/// ERROR_NOT_FOUND on 8 of 10 interfaces. A `?` on that leg would abort the whole flip
/// AND discard the successful IPv4 change.
pub fn steer(luid: NET_LUID_LH, target: Option<u32>) -> SteerOutcome {
    SteerOutcome {
        v4: set(luid, AF_INET,  target),
        v6: set(luid, AF_INET6, target),
    }
}
// Overall success == v4 is Applied. v6 FamilyAbsent is fine and must be surfaced as
// "IPv6 not bound on this adapter", not as an error.
```

**Read-modify-write side effect to be aware of:** `SetIpInterfaceEntry` writes back the *whole* row, so it re-asserts `NlMtu`, `DadTransmits`, `RouterDiscoveryBehavior`, `LinkLocalAddressBehavior` etc. as explicit values. The highest-risk one is `NlMtu` (persistent store shows "Default", the active row reads 1500). This is inherent to the API and is what `netsh`/`Set-NetIPInterface` do too. No mitigation needed beyond noting it; diff the row before/after in T2. Fields that `Set` **ignores** (no save/restore effort needed): `MaxReassemblySize`, `Min/MaxRouterAdvertisementInterval`, `Connected`, `SupportsWakeUpPatterns`, `SupportsNeighborDiscovery`, `SupportsRouterDiscovery`, `ReachableTime`, `TransmitOffload`, `ReceiveOffload`.

**Bulk read for the UI (unprivileged, one call):**

```rust
let mut table: *mut MIB_IPINTERFACE_TABLE = std::ptr::null_mut();
if unsafe { GetIpInterfaceTable(AF_UNSPEC, &mut table) } == NO_ERROR && !table.is_null() {
    let n = unsafe { (*table).NumEntries } as usize;
    // Table is `[MIB_IPINTERFACE_ROW; 1]` — the C ANYSIZE_ARRAY idiom. Indexing beyond 0
    // is a bounds-check panic. Walk it as a slice built from the raw pointer.
    let rows = unsafe { std::slice::from_raw_parts((*table).Table.as_ptr(), n) };
    for r in rows { /* r.InterfaceIndex, r.Family, r.Metric, r.UseAutomaticMetric, r.Connected */ }
    unsafe { FreeMibTable(table as *const core::ffi::c_void) };  // mandatory
}
// AF_UNSPEC is legal ONLY for the table call, never for the single-entry Get/Set.
// Rows come back grouped by family here, not interleaved — do not rely on any ordering;
// key on (InterfaceIndex|LUID, Family).
```

### 4.4 Who is actually winning (`net/routes.rs`)

**Do not use a bare `GetBestRoute2`.** On this machine `GetBestRoute2(luid=None, index=0, ..., 8.8.8.8)` returns ifIndex 43 = ProtonVPN with source 10.0.0.2 — neither candidate. It also violates the documented contract, which requires at least one of `InterfaceLuid`/`InterfaceIndex` to be initialized.

Derive everything from `GetIpForwardTable2`:

```rust
pub struct DefaultRoute {
    pub luid: u64,
    pub if_index: u32,
    pub route_metric: u32,          // MIB_IPFORWARD_ROW2.Metric  (the OTHER half of the sum)
    pub iface_metric: u32,          // MIB_IPINTERFACE_ROW.Metric for (luid, family)
    pub total: u32,                 // route_metric + iface_metric  <-- what Windows compares
    pub family: ADDRESS_FAMILY,
}

pub enum Verdict {
    Ethernet { total: u32 },
    Wifi     { total: u32 },
    /// A third interface (VPN/tunnel/virtual) holds the winning default route.
    Hijacked { luid: u64, if_index: u32, name: String, total: u32 },
    /// Neither candidate has a default route at all (cable out AND Wi-Fi down).
    None,
}
```

Algorithm, per family:
1. `GetIpForwardTable2(family, &mut table)`; walk `Table[0..NumEntries]` via `slice::from_raw_parts` (field is `NumEntries`, not `dwNumberOfItems`); keep rows with `DestinationPrefix.PrefixLength == 0`. `FreeMibTable`.
2. For each, look up the interface metric for `(row.InterfaceLuid, family)` from the `GetIpInterfaceTable` snapshot; `total = row.Metric + iface_metric`.
3. Global winner = lowest `total`. If its LUID is neither candidate → `Hijacked`.
4. Otherwise `Ethernet`/`Wifi` per the lowest `total` **among the two candidates**.

Two consequences the UI must respect:

- **An unplugged interface has no default route at all.** `GetIpForwardTable2(AF_INET)` returned 37 rows on the dev machine with exactly two `0.0.0.0/0` entries (Wi-Fi and the VPN) — Ethernet was simply absent. So the two widget rows are built from **adapter enumeration**, and the route table is only an overlay that annotates which one wins.
- **`Hijacked` is a feature, not an error.** Show "Internet currently egresses via ProtonVPN — switching Ethernet/Wi-Fi will not change your public IP." This is exactly what stops a user filing "LinkSwitch does nothing" bugs.

LUID comparison must go through `u64`:
```rust
#[inline] pub fn luid_u64(l: NET_LUID_LH) -> u64 { unsafe { l.Value } }
// NET_LUID_LH is `union { Value: u64, Info: NET_LUID_LH_0 }` with NO PartialEq —
// `a == b` on two of them is error[E0369].
```

### 4.5 Live updates (`net/notify.rs`)

Register **all three**. Our own `SetIpInterfaceEntry` fires `NotifyIpInterfaceChange` (a parameter notification) but does **not** add/remove a forwarding row, so `NotifyRouteChange2` alone would miss the widget's own action. VPN connect/cable pull fires the route one; DHCP/association fires the address one.

```rust
static REPAINT: OnceLock<egui::Context> = OnceLock::new();
static DIRTY:   AtomicBool = AtomicBool::new(true);

unsafe extern "system" fn on_iface(_c: *const c_void, _r: *const MIB_IPINTERFACE_ROW, _t: MIB_NOTIFICATION_TYPE) {
    DIRTY.store(true, Ordering::Relaxed);
    if let Some(c) = REPAINT.get() { c.request_repaint(); }
}
// ... on_route (MIB_IPFORWARD_ROW2), on_addr (MIB_UNICASTIPADDRESS_ROW) identical.

unsafe {
    NotifyIpInterfaceChange(AF_UNSPEC, Some(on_iface), None,                 true, &mut h1);
    NotifyRouteChange2     (AF_UNSPEC, Some(on_route), std::ptr::null(),     true, &mut h2);
    NotifyUnicastIpAddressChange(AF_UNSPEC, Some(on_addr), None,             true, &mut h3);
}
```

Gotchas baked in:
- `NotifyRouteChange2`'s `callercontext` is a **bare `*const c_void`**; the other two take `Option<*const c_void>`. Unifying the three registrations into one helper gives `error[E0308]`.
- With `initialnotification = true` the callback fires once immediately with `row = NULL` and `MibInitialNotification` — that is a registration ack, not a change.
- The row pointers are **OS-owned and only partially populated**. Never `FreeMibTable` them; never treat non-LUID/Family fields as authoritative. Which is why the callbacks do nothing but set a flag.
- `CancelMibChangeNotify2` **must not** be called from inside a callback or from a thread the callback thread is waiting on — documented deadlock. Call it only from `eframe::App::on_exit`.
- Do zero work in the callback: it runs on an IP Helper worker thread with undocumented reentrancy constraints. Re-query on the next egui frame.

### 4.6 Wi-Fi association (`net/wifi.rs`)

```rust
pub fn connect_auto(timeout: Duration) -> Result<(), WifiError> {
    // WlanOpenHandle(2, None, &mut negotiated, &mut h)
    // WlanEnumInterfaces -> WLAN_INTERFACE_INFO_LIST
    //   InterfaceInfo is a flexible array member rendered as a fixed placeholder —
    //   index with .as_ptr().add(i) using dwNumberOfItems (NOT NumEntries).
    // If isState == wlan_interface_state_connected -> Ok(())
    // else WlanConnect(h, &guid, &WLAN_CONNECTION_PARAMETERS {
    //          wlanConnectionMode: wlan_connection_mode_auto,   // Windows picks from the profile list
    //          strProfile: PCWSTR::null(),
    //          pDot11Ssid: null(), pDesiredBssidList: null(),
    //          dot11BssType: dot11_BSS_type_any, dwFlags: 0,
    //      }, None)
    // Poll WlanQueryInterface(wlan_intf_opcode_interface_state) every 250 ms until
    // connected AND the adapter has a non-APIPA IPv4 address, or timeout.
    // WlanFreeMemory every returned pointer; WlanCloseHandle at the end.
}
```

Fallback if `WlanConnect` returns an error: shell out to
`netsh wlan connect name="<profile>" interface="<alias>"` with `CREATE_NO_WINDOW`, using the profile recorded in `MachineConfig::wifi_profile_hint` (captured the last time Wi-Fi was seen connected). Both paths are in the elevated worker.

`status()` (read-only, used by the widget for the SSID/bars label) uses `wlan_intf_opcode_current_connection` → `WLAN_CONNECTION_ATTRIBUTES.wlanAssociationAttributes.{dot11Ssid, wlanSignalQuality}`.

### 4.7 WCM policy (`net/wcm.rs`)

```rust
pub enum MinimizePolicy { Allow = 0, Minimize = 1, MinimizeCellular = 2, PreventWifi = 3 }

/// Authoritative for the ENUM. Absent => fall back to WcmQueryProperty's effective default.
pub fn effective() -> (MinimizePolicy, bool /* is_group_policy */) {
    // 1. RegGetValueW(HKEY_LOCAL_MACHINE,
    //      "SOFTWARE\\Policies\\Microsoft\\Windows\\WcmSvc\\GroupPolicy",
    //      "fMinimizeConnections", RRF_RT_REG_DWORD, ...)
    //    -> if present, that value IS the enum. Pair with WcmQueryProperty's fIsGroupPolicy.
    // 2. If ERROR_FILE_NOT_FOUND: WcmQueryProperty(None, PCWSTR::null(),
    //      wcm_global_property_minimize_policy, None, &mut size, &mut data)
    //    -> WCM_POLICY_VALUE { fValue: BOOL, fIsGroupPolicy: BOOL }, 8 bytes.
    //    On this machine with the value absent: (1, 0). Free with WcmFreeMemory on EVERY
    //    path where ppdata came back non-null.
    // NEVER treat "value absent" as Allow.
}
```

Writing (opt-in, elevated only): back up `{ present: bool, value: u32 }` into `MachineConfig::wcm_backup`, then `RegSetKeyValueW(... REG_DWORD 0)`. Uninstall restores exactly — including deleting the value if it was originally absent. Tell the user a reboot (or at minimum a Wi-Fi reconnect) is the reliable way to see the change take effect.

---

## 5. ELEVATION DESIGN

### 5.1 The shape

- **The exe manifest is `asInvoker`.** It must be: the same binary is the unelevated widget. A `requireAdministrator` manifest would UAC-prompt on every plain launch and destroy the design.
- **Four fixed-verb scheduled tasks**, `RunLevel = HighestAvailable`, `LogonType = InteractiveToken`. The registering user is the principal, so the *unelevated* widget — same user SID under a filtered token — can call `Run` on tasks it created while elevated ("By default, a user who creates a task can read, update, delete, and run the task").
- **`--install` is the only UAC prompt.** It self-elevates via `ShellExecuteExW` `runas`. Registering a `HIGHEST` task from a medium-IL process is impossible ("From a low privilege process, you cannot register a task with the RunLevel property equal to TASK_RUNLEVEL_HIGHEST"), so this is mandatory, not stylistic.

### 5.2 Why FOUR fixed tasks and not one parameterized task

`schtasks.exe /Run` **cannot pass arguments** — its usage line has no parameter for them (verified on this machine). `$(Arg0)` substitution is fed only by `IRegisteredTask::Run`/`RunEx`.

More importantly, **a parameterized elevated task is a medium-IL → high-IL escalation bridge for every process running as that user.** Fixed verbs remove the injection surface entirely:

| Task URI | Arguments | Triggers |
|---|---|---|
| `\LinkSwitch\ApplyEthernet` | `--apply ethernet` | none (on-demand) |
| `\LinkSwitch\ApplyWifi` | `--apply wifi` | none (on-demand) |
| `\LinkSwitch\ApplyAuto` | `--apply auto` | none (on-demand) |
| `\LinkSwitch\Restore` | `--apply restore` | LogonTrigger, 15 s delay |

Additional hardening:
- Exe lives at `%ProgramFiles%\LinkSwitch\linkswitch.exe` (medium IL cannot write it). Never point a task at `cmd.exe`/`powershell.exe`, and never at a path under `%LOCALAPPDATA%`.
- The elevated worker reads config only from `%ProgramData%\LinkSwitch\config.json` with an explicit DACL (Administrators + SYSTEM full, Users read-only). The unelevated widget writes **nothing** the worker consumes.
- Adapter re-selection is a rare elevated operation (`--set-adapters`, self-elevating, one UAC prompt), not an unelevated config write.
- The park metric is validated against a hardcoded allowlist in the worker regardless of what the config says.

### 5.3 Task XML (verbatim template)

`{USER}` = `%USERDOMAIN%\%USERNAME%` of the installing user; `{EXE}` = install path. Registered via `ITaskFolder::RegisterTask(path, xml, TASK_CREATE_OR_UPDATE.0, VARIANT::default(), VARIANT::default(), TASK_LOGON_INTERACTIVE_TOKEN, VARIANT::default())`, so what ships is literally what runs.

```xml
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>LinkSwitch</Author>
    <URI>\LinkSwitch\ApplyWifi</URI>
    <Description>LinkSwitch: route internet traffic over Wi-Fi by raising the Ethernet interface metric. Ethernet stays connected and link-up.</Description>
  </RegistrationInfo>
  <Triggers />
  <Principals>
    <Principal id="Author">
      <UserId>{USER}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <!-- Schema default is TRUE for both. Left alone, the widget silently does nothing on
         an unplugged laptop — precisely when a user switches to Wi-Fi. -->
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <!-- Schema default IgnoreNew silently DROPS a second click while the first worker runs. -->
    <MultipleInstancesPolicy>Queue</MultipleInstancesPolicy>
    <!-- Must be true or IRegisteredTask::Run is refused. -->
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <!-- Default is 72 HOURS. -->
    <ExecutionTimeLimit>PT1M</ExecutionTimeLimit>
    <AllowHardTerminate>true</AllowHardTerminate>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <StartWhenAvailable>false</StartWhenAvailable>
    <WakeToRun>false</WakeToRun>
    <DisallowStartOnRemoteAppSession>false</DisallowStartOnRemoteAppSession>
    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>
    <Hidden>false</Hidden>
    <Enabled>true</Enabled>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{EXE}</Command>
      <Arguments>--apply wifi</Arguments>
      <WorkingDirectory>{EXEDIR}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
```

`ApplyEthernet` / `ApplyAuto` differ only in `<URI>`, `<Description>`, `<Arguments>`.

`Restore` additionally carries:

```xml
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{USER}</UserId>
      <Delay>PT15S</Delay>
    </LogonTrigger>
  </Triggers>
```
with `<Arguments>--apply restore</Arguments>`.

**Why `Restore` exists:** whether `SetIpInterfaceEntry` writes the persistent store is genuinely unresolved — the evidence is contradictory (`Set-NetIPInterface`'s own doc states two mutually exclusive defaults; `Get-NetIPInterface -PolicyStore PersistentStore` shows a blank metric for every interface on this machine; yet ifIndex 39/43 have carried `UseAutomaticMetric=false` across reboots). A logon re-apply is correct under **either** answer and costs nothing. Ship it unconditionally; T7 tells you whether it was needed.

Optional second trigger, for cable replug (add only if T8 shows metrics reset on replug):
```xml
    <EventTrigger>
      <Enabled>true</Enabled>
      <Subscription>&lt;QueryList&gt;&lt;Query Id="0" Path="Microsoft-Windows-NetworkProfile/Operational"&gt;&lt;Select Path="Microsoft-Windows-NetworkProfile/Operational"&gt;*[System[EventID=10000]]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;</Subscription>
      <Delay>PT5S</Delay>
    </EventTrigger>
```

### 5.4 The apply sequence (`apply.rs`)

```
apply(mode):
  0. log start; load MachineConfig; validate park_metric ∈ {9000} (allowlist)
  1. resolve both LUIDs -> if GetIpInterfaceEntry gives ERROR_FILE_NOT_FOUND(2):
        write state.json { error: AdapterGone }, exit 2
  2. if mode == Wifi:
        wifi::status(); if not connected -> wifi::connect_auto(12s)
        if still not connected -> write state.json { error: WifiUnavailable }, exit 3
        (ABORT BEFORE TOUCHING METRICS — never park Ethernet with no Wi-Fi to fall back to)
  3. metric::steer(loser_luid, Some(9000))     // v4 + v6
     metric::steer(winner_luid, None)          // restore automatic on the winner
  4. re-read via GetIpInterfaceTable + routes::winner(); confirm the intended candidate wins
  5. write state.json { mode, applied_at, v4/v6 outcomes per NIC, verdict }
  6. exit 0 on success, non-zero with a distinct code per failure class
```

Mode → (loser, winner):

| Mode | Park at 9000 | Restore to automatic |
|---|---|---|
| `wifi` | Ethernet | Wi-Fi |
| `ethernet` | Wi-Fi | Ethernet |
| `auto` | — | both |
| `restore` | dispatch to the mode recorded in `state.json` |

**The symmetry is load-bearing.** "Ethernet mode = set both to automatic" is wrong: on a 100 Mbps Ethernet port the automatic metric is 35 against Wi-Fi's 30, so the Ethernet button would silently route over Wi-Fi. Automatic metrics are *not* fixed — this machine's Ethernet reads 5 only because the cable is out (`TransmitLinkSpeed = u64::MAX` lands in the ">= 100 Gb" bucket); a plugged 2.5 GbE I225-V reads **20**, against Wi-Fi's 30. Never precompute a delta.

**Park value = 9000.** Above every automatic bucket (max published is 85), above Hyper-V's 5000, and below the 9999 the adapter-properties GUI conventionally displays. Note there is no documented ceiling — `-InterfaceMetric` is a bare `UInt32` with no `ValidateRange` — so 9000 is chosen for clearance, not because of a limit.

### 5.5 How the widget triggers, and how success is confirmed

Triggering, from a **dedicated short-lived thread** (never the UI thread — winit already initialized COM there for `set_skip_taskbar`, and we must not fight its apartment):

```rust
std::thread::spawn(move || unsafe {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);   // tolerate RPC_E_CHANGED_MODE
    let svc: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
    svc.Connect(&VARIANT::default(), &VARIANT::default(),
                &VARIANT::default(), &VARIANT::default())?;
    let folder = svc.GetFolder(&BSTR::from("\\LinkSwitch"))?;
    let task   = folder.GetTask(&BSTR::from("ApplyWifi"))?;
    let _running = task.Run(&VARIANT::default())?;            // fire-and-forget
    // then poll:
    for _ in 0..30 {                                          // 3 s budget
        std::thread::sleep(Duration::from_millis(100));
        let rc = task.LastTaskResult()?;                      // i32
        if rc != 0x00041301 /* SCHED_S_TASK_RUNNING */ { tx.send(rc); return Ok(()); }
    }
    tx.send(TIMEOUT);
});
```

Confirmation is **three-layered, and the live routing table is the only source of truth**:

1. `IRegisteredTask::LastTaskResult()` — process exit code. `0x80070002` (ERROR_FILE_NOT_FOUND) means the task's action path is stale (user moved/reinstalled LinkSwitch); the widget already self-checks this at startup by comparing `tasks::action_path()` against `std::env::current_exe()` and offers a one-click elevated re-install.
2. `state.json` — the worker's own structured report (per-family outcomes, Wi-Fi association result, error class). Drives the error text.
3. **`GetIpInterfaceTable` + `routes::winner()` re-read** — display truth, not intent. `Notify*Change` will have already fired a repaint; the widget confirms the verdict flipped within ~1 s and otherwise shows "applied, but the route did not change" with the `Hijacked` reason if there is one.

**Fallbacks**, in order:
1. If `Run` fails with `E_ACCESSDENIED`/task-missing → offer "Repair install" (`--install`, self-elevating).
2. If `ITaskService` COM fails entirely → `schtasks.exe /Run /TN "\LinkSwitch\ApplyWifi"` spawned with `CREATE_NO_WINDOW` (no arguments needed — the verb is baked into the task).
3. If tasks are unusable (e.g. Task Scheduler service disabled) → the widget offers a per-click elevated run: `elevate::relaunch_elevated(["--apply", "wifi"])`, which UAC-prompts each time. Degraded, honest, and functional.

**If T4 shows a `HighestAvailable` task token does not satisfy `SetIpInterfaceEntry`** (the API docs mention a `requireAdministrator` manifest clause): ship a second tiny binary `linkswitch-worker.exe` with a `requireAdministrator` manifest, point the four task actions at it, and keep `linkswitch.exe` at `asInvoker`. Because the task supplies the elevated token, no UAC prompt ever appears. Design the workspace as two bin targets from day one so this is a one-line change.

---

## 6. UI DESIGN

### 6.1 What it shows

A 300×150 borderless, transparent, always-on-top card:

```
┌──────────────────────────────────────────┐
│  LinkSwitch                          ─ ✕ │
│                                          │
│  ● Ethernet     cable in   metric 9000   │
│  ◉ Wi-Fi        MyNet ▂▄▆  metric 30     │
│                                          │
│  ┌────────────┐ ┌────────────┐ ┌──────┐  │
│  │  Ethernet  │ │   Wi-Fi    │ │ Auto │  │
│  └────────────┘ └────────────┘ └──────┘  │
│                                          │
│  ⚠ ProtonVPN is carrying all traffic     │
└──────────────────────────────────────────┘
```

Per row: name, **cable/association state** (`MediaConnectState` for Ethernet — the whole product premise is "the cable stays in", so *"deprioritized by LinkSwitch (metric 9000)"* must be visually distinct from *"cable unplugged"*; those look identical if you only watch the route), SSID + signal quality for Wi-Fi, current **effective total metric** (route + interface), and a filled/hollow dot for the winner.

Banners, in priority order:
1. `Hijacked` → "⚠ *{name}* is carrying all traffic. Switching Ethernet/Wi-Fi will not change your public IP."
2. `MinimizePolicy::PreventWifi` (value 3) or `is_group_policy` → "⚠ Group Policy prevents Wi-Fi while Ethernet is connected. LinkSwitch cannot switch on this machine."
3. `MinimizePolicy::Minimize` (the default) → an unobtrusive info chip: "Windows may disconnect Wi-Fi when idle. LinkSwitch reconnects it on demand. *Keep it connected →*" linking to the opt-in.
4. Apply in flight / apply failed with the worker's message.
5. `DisableDefaultRoutes == true` on a candidate → "A VPN has suppressed this interface's default route; metric steering will have no effect on it." (One extra field read from a row you already fetched — and without it, users file bugs against LinkSwitch for a VPN's split-tunnel setting.)

Persistent footer text, always visible, once: *"Applies to new connections. DNS lookups are still emitted on both links."*

### 6.2 Interaction model

- **Whole surface is a drag handle**, in this exact order (later widgets win egui's interaction contest — reversing these two statements silently breaks every button):

```rust
let body = ui.max_rect();
let bg = ui.interact(body, egui::Id::new("ls_drag"), egui::Sense::click_and_drag());
if bg.drag_started_by(egui::PointerButton::Primary) {
    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
}
ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| { /* rows + buttons here */ });
```

- **Not resizable.** `with_resizable(false)`. There is no built-in resize grip on an undecorated egui viewport — the official `custom_window_frame` example implements dragging only — and hand-rolling `ViewportCommand::BeginResize` edge/corner hit-testing is not worth it for a fixed-size toggle.
- Window position persisted to `%APPDATA%\LinkSwitch\prefs.json` on `on_exit` and on drag end. Not eframe's `persistence` feature: `--apply`/`--install` never call `run_native`, so they structurally cannot reach eframe's `Storage`. (Also note `eframe::storage_dir(id)` is `%APPDATA%\<id>\data` — a *sibling* of our path, not the same directory.)
- Buttons are disabled while an apply is in flight, and while `PreventWifi` blocks the Wi-Fi button.

### 6.3 eframe 0.36.1 App trait — the breaking change

`App::update` **does not exist** in 0.36. Verified independently by two compile runs (`E0407: method 'update' is not a member of trait 'eframe::App'`, `E0046: missing 'ui'`, `E0308` on `CentralPanel::show(ctx, ..)`). Essentially every egui tutorial online predates this.

```rust
impl eframe::App for LinkSwitchApp {
    /// Required for real transparency; the ViewportBuilder flag alone leaves an opaque fill.
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] { egui::Rgba::TRANSPARENT.to_array() }

    /// Provided/optional, and it KEEPS TICKING WHILE THE WINDOW IS HIDDEN — eframe 0.36.1
    /// runs `update_logic_only()` for invisible windows ("The app logic keeps ticking, so it
    /// can e.g. ask to be shown again") and repaints them directly at a 100 ms interval so
    /// viewport commands like Visible(true) are still processed.
    /// This is where tray events and apply-result polling go.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.pump_tray_events(ctx);
        self.pump_apply_results();
        if DIRTY.swap(false, Ordering::Relaxed) { self.snapshot = Snapshot::read(); }
        self.hwnd_once(frame);
    }

    /// REQUIRED. Note: this Ui has no margin and no background — wrap it yourself, or on a
    /// transparent window you get an unpainted void that looks exactly like the glow
    /// transparency bug you were told to hunt for.
    fn ui(&mut self, ui_root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let panel = egui::Frame::new()
            .fill(egui::Color32::from_rgba_unmultiplied(20, 22, 28, 235))
            .corner_radius(12.0)          // 0.36 name; was `rounding`
            .inner_margin(12.0);
        egui::CentralPanel::default().frame(panel).show(ui_root, |ui| self.draw(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        net::notify::unregister(self.notify.take());   // never from inside a callback
        config::save_prefs(&self.prefs);
    }
}
```

Viewport:

```rust
egui::ViewportBuilder::default()
    .with_inner_size([300.0, 150.0])
    .with_decorations(false)
    .with_transparent(true)
    .with_always_on_top()
    .with_resizable(false)
    .with_taskbar(false)          // ITaskbarList::DeleteTab — taskbar button only
    .with_drag_and_drop(false)
    .with_position(prefs.pos())
```

**`with_taskbar(false)` does NOT remove the Alt-Tab entry.** winit implements it as COM `ITaskbarList::DeleteTab`, not `WS_EX_TOOLWINDOW` (grepping winit 0.30.13 finds `WS_EX_TOOLWINDOW` only on its own internal message-target window). It also silently no-ops if `CoCreateInstance` fails. For a true widget, apply the style once from `ui/hwnd.rs` after first frame:

```rust
unsafe {
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE,
        ((ex | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0) as isize);
}
```
HWND from `frame.window_handle()` → `RawWindowHandle::Win32(h)` → `HWND(h.hwnd.get() as _)`. (raw-window-handle 0.6.2, `hwnd: NonZeroIsize` — confirmed via eframe 0.36.1 → winit 0.30.13.)

**Cosmetic expectations, so nobody chases ghosts:** eframe unconditionally sets `with_undecorated_shadow(true)` for undecorated Windows viewports, which winit implements as a `WM_NCCALCSIZE` hack whose own source comment says it "leads to a small black 1px border on the top." And the DWM shadow is **rectangular** while `corner_radius(12)` content is rounded — the shadow will not hug the corners. Both are upstream, both are acceptable, neither is a bug in LinkSwitch.

### 6.4 Idle-CPU strategy

**Never call `request_repaint` on a timer in the visible state.** eframe is reactive: with no repaint requested it sits in `ControlFlow::Wait`, fully blocked, at ~0% CPU. All updates are event-driven from the three `Notify*Change` callbacks plus the tray handler. (Both historically cited idle-CPU issues are closed: egui #3982 closed 2024-02-09, winit #1610 closed 2024-10-28, and eframe 0.36.1 pins winit 0.30.13 — there is no known open defect to soak-test for. A 2-minute Task Manager check in T0 is still worth doing.)

The only timed repaint is while an apply is in flight: `ctx.request_repaint_after(100ms)` for at most 3 s, then stop.

### 6.5 Tray behaviour

```rust
eframe::run_native("LinkSwitch", opts, Box::new(move |cc| {
    // Build the tray icon HERE — inside the app-creator closure, which runs after the
    // event loop is up on the message-pumping thread. On Windows the only hard rule is
    // same-thread-as-the-pump (the "must be the main thread" rule is macOS-only).
    let icon = tray_icon::Icon::from_resource(1, None)?;   // resource id 1 from app.rc
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("LinkSwitch")
        .build()?;

    // PUSH, not poll. A correctly reactive widget sits blocked in ControlFlow::Wait and
    // would NEVER observe a tray click via TrayIconEvent::receiver().try_recv().
    // set_event_handler is OnceLock-backed: settable exactly once, and once set,
    // receiver() gets nothing.
    let ctx = cc.egui_ctx.clone();                          // egui::Context is Send+Sync+cheap-clone
    let (tx, rx) = std::sync::mpsc::channel();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |ev| {
        let _ = tx.send(ev);
        ctx.request_repaint();
    }));
    Ok(Box::new(LinkSwitchApp::new(cc, tray, rx)))
}))
```

- **Hide/show via `ViewportCommand::Visible(false)` / `(true)`.** Do **not** use the raw `ShowWindow(SW_HIDE)` workaround from older discussions: winit caches visibility in `WindowFlags::VISIBLE` and `apply_diff` early-returns on an empty diff, so a raw hide permanently desyncs the flag and a later `Visible(true)` becomes a silent no-op — the window can never be restored. The upstream issues (#5229, #7776) were fixed 2026-03-24, and 0.36.1 has a purpose-built hidden-window tick.
- Because `Visible(true)` goes through `SW_SHOWNOACTIVATE`, follow it with `ViewportCommand::Focus` if you want keyboard focus.
- Tray-click latency while hidden is up to ~100 ms (the invisible-window repaint clamp). Fine.
- **Never send `ViewportCommand::Close` from the widget's ✕.** `run_and_return` defaults to `true`, so closing the window returns from `run_native` and stops the message pump, which kills the tray icon's message-only window with it. ✕ = `Visible(false)`. Quit is a separate action.
- No `muda` context menu in v1 (avoids an unverified dependency version). Left-click toggles visibility; Quit lives in the widget. If a menu is added later, enable tray-icon's `common-controls-v6` feature — `new_manifest()` already declares a Common-Controls 6.0.0.0 dependency.
- Explorer restart is already handled: tray-icon's Windows backend registers `TaskbarCreated` via `RegisterWindowMessageA` and calls `ChangeWindowMessageFilterEx(..., MSGFLT_ALLOW, ...)` so it re-registers. Do not reimplement.
- `Icon::from_resource(1, None)` resolves to `LoadImageW(..., LR_DEFAULTSIZE)` = the **large** `SM_CXICON` size (32px at 100%). If it looks soft at 150% scaling, pass `Some((GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)))`. Keep the `TrayIcon` alive — its `Drop` calls `DestroyIcon`.

### 6.6 Console for CLI subcommands

```rust
fn attach_console_if_cli() {
    if std::env::args().len() <= 1 { return; }   // plain widget launch stays silent
    unsafe { let _ = AttachConsole(ATTACH_PARENT_PROCESS); }
    // That is ALL that is needed. Measured on this machine: AttachConsole alone populated
    // STD_OUTPUT_HANDLE with a live console handle (GetFileType == FILE_TYPE_CHAR) and
    // println! worked — no CreateFileW("CONOUT$"), no SetStdHandle, no Win32_Storage_FileSystem.
    // Microsoft documents exactly this: std handles "will likely be invalid on startup
    // until AttachConsole is called."
    // Ordering is also not load-bearing: Rust's std re-reads GetStdHandle on each write.
}
```

`AttachConsole` fails with `ERROR_INVALID_HANDLE` when the parent has no console — which is the Task Scheduler `--apply` case. That is why **the worker's diagnostics channel is the log file**, not stdout. Also note handles are already valid when the parent redirects (`> file`, `| pipe`), so `linkswitch --install > out.txt` works with or without the attach.

---

## 7. RISK REGISTER (prioritised)

| # | Risk | Likelihood / impact | Mitigation |
|---|---|---|---|
| **R1** | **WCM blocks Wi-Fi auto-connect / soft-disconnects it while Ethernet is up.** Default `fMinimizeConnections = 1`, proven twice on this box with no registry value. Without a fix, the Wi-Fi button has nothing to switch to after every boot, and Wi-Fi silently vanishes ~30 s after traffic quiesces. | **Certain** / product-breaking | Worker calls `wifi::connect_auto()` before every switch-to-Wi-Fi and aborts the flip if it fails (manual connections are on WCM's keep-list). Plus opt-in `fMinimizeConnections=0` with backup/restore, GP and value-3 detection, STIG disclosure. Never read "value absent" as "disabled". |
| **R2** | **A `HighestAvailable` task token may not satisfy `SetIpInterfaceEntry` without a `requireAdministrator` manifest** (the API doc names the manifest explicitly, and the single-binary design cannot carry it). | Low / total | **T4 is the gating test, run before any UI work.** Fallback designed in: a second `linkswitch-worker.exe` bin target carrying `requireAdministrator`, used as the task action. No UAC prompt either way. Structure the workspace with two bin targets from day one. |
| **R3** | **A VPN owns the default route**, so the switch appears to do nothing. Reproduced here: ProtonVPN holds `0.0.0.0/0` at route metric 0 + interface metric 0. | High (VPN users) / trust | Never derive the winner from a bare `GetBestRoute2`. `routes::winner()` returns `Hijacked` and the UI names the interface. Also read `DisableDefaultRoutes` on both candidates and surface it. Additionally note the VPN's pinned `/32` server host-route stays on whichever NIC was live at connect time and does **not** follow a metric flip. |
| **R4** | **glow renders the transparent window solid black** (egui #4451 and #5512 are open upstream). | Medium / cosmetic | **T1 first.** If it reproduces: flip one line to `features=["wgpu","default_fonts"]` (eframe's own default, 8 MB larger), or set `with_transparent(false)` + opaque `clear_color` and accept square corners. Before blaming the renderer, rule out the self-inflicted case: `App::ui`'s `Ui` has no background, so forgetting the `egui::Frame` wrapper looks identical. |
| **R5** | **Metric writes may not survive reboot** (`SetIpInterfaceEntry` has no store parameter; `Set-NetIPInterface` docs state two contradictory defaults; the persistent store reads blank on this machine). | Medium / annoyance | Ship the `\LinkSwitch\Restore` logon task unconditionally — correct under either answer. T7 measures it. |
| **R6** | **IPv6 rows absent** → a `?` on the v6 leg aborts the flip and discards the successful IPv4 change. 8 of 10 interfaces here return `ERROR_NOT_FOUND (1168)`; `ms_tcpip6` is unbound on both physical NICs. | Certain on this machine / total if unhandled | `FamilyOutcome::FamilyAbsent` is success. Overall verdict keyed on the v4 leg. UI shows "IPv6 not bound on this adapter". Distinguish 1168 (family unbound — fine) from 2 (adapter gone — reconfigure). |
| **R7** | **Existing TCP connections do not migrate**; long-lived apps (Teams, Steam, RDP, SSH) sit on the old link for hours. | Certain / support load | Permanent UI copy. Nothing breaks because Ethernet stays up — this is the *opposite* of the usual complaint and just needs one sentence. |
| **R8** | **DNS still egresses Ethernet** (parallel DNS/LLMNR/NetBT across all networks, by design, even with the "turn off" policy). | Certain / expectation | Never advertise "zero traffic on Ethernet". A DNS-blanking "strict mode" is explicitly **out of scope for v1** — it is invasive, must be restored on flip-back, and strands the user if the app crashes mid-flip. |
| **R9** | **Saved adapter LUID goes stale** (driver reinstall, adapter removed) → `ERROR_FILE_NOT_FOUND (2)`. | Low / recoverable | Persist LUID (`u64`), never ifIndex ("may change when a network adapter is disabled and then enabled"). On code 2, the widget shows "Your saved adapter is gone — reconfigure" and offers `--set-adapters`. |
| **R10** | **Task action path stale** after the user moves/reinstalls → `LastTaskResult = 0x80070002`, buttons silently do nothing. | Medium / silent | At widget startup, compare `tasks::action_path()` against `std::env::current_exe()`; on mismatch show "Repair install". |
| **R11** | **Elevated task as a UAC-bypass bridge**; also the "OSS project shipping a privilege escalation" reputational risk. | Medium / severe | Four fixed-verb tasks with no caller-supplied arguments; exe in `%ProgramFiles%`; config in `%ProgramData%` with an Administrators-write DACL; park value validated against a hardcoded allowlist in the worker; adapter re-selection is its own elevated verb. |
| **R12** | **Tray clicks missed.** Polling `TrayIconEvent::receiver()` from a reactive app that never repaints never sees them. | High if polled / product-breaking | `set_event_handler` push + `ctx.request_repaint()`. Set exactly once (OnceLock). Never send `Close` from ✕ (`run_and_return=true` would stop the pump and kill the tray). |
| **R13** | **Automatic metric is not what you measured.** Ethernet reading 5 here is the "link speed unknown / cable out" bucket; plugged it becomes 20 (2.5 GbE) or 25/35 (1 GbE/100 Mbps), and Wi-Fi's own metric floats 30↔35 as the 802.11 rate renegotiates (half-duplex halves the effective speed). | High / silent wrong routing | Symmetric park-the-loser design; never hardcode a winner or a delta; always re-read and verify the verdict after applying. Treat "Ethernet metric == 5" as a hint the link is **down**. |
| **R14** | **STIG / managed machines** (`fMinimizeConnections = 3`) — Wi-Fi cannot connect at all while Ethernet is present, and any policy write is reverted at the next `gpupdate`. | Low (consumer) / total on those boxes | Detect `is_group_policy` and value 3; disable the Wi-Fi button and explain rather than failing silently. |
| **R15** | **Route metric offset non-zero.** A user's `route -p add` (route metric 256) or a third-party tool defeats a pure interface-metric comparison. | Low / wrong readout | Always compute and display **route metric + interface metric**, per the documented rule. |
| **R16** | **`Set-NetIPInterface` with no targeting parameter modifies every interface on the box** ("including virtual interfaces and loopback interfaces"). | Low / catastrophic | Any PowerShell fallback or docs snippet must pass a validated numeric `-InterfaceIndex` **and** `-AddressFamily`. Prefer index over alias (aliases are user-renamable and localized). Better: don't shell out at all — the API path is primary. |
| **R17** | **Alignment UB** casting `Vec<u8>` to `*mut IP_ADAPTER_ADDRESSES_LH` (align 8, size 448). Works today only because the allocator happens to return aligned blocks. | Low / latent | `Vec<u64>` buffer, as in §4.2. This is also why Microsoft's own C sample uses `HeapAlloc`. |

---

## 8. BUILD / VERIFICATION PLAN

**Before anything else — write the panic button.** Save `restore.ps1` beside the repo and keep an elevated PowerShell window open during every test from T4 onward:

```powershell
# restore.ps1 — undo everything LinkSwitch can do to this machine.
foreach ($i in Get-NetIPInterface) {
  try { Set-NetIPInterface -InterfaceIndex $i.ifIndex -AddressFamily $i.AddressFamily `
                           -AutomaticMetric Enabled -ErrorAction Stop } catch {}
}
Remove-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WcmSvc\GroupPolicy' `
                    -Name fMinimizeConnections -ErrorAction SilentlyContinue
schtasks /Delete /TN "\LinkSwitch\ApplyEthernet" /F 2>$null
schtasks /Delete /TN "\LinkSwitch\ApplyWifi"     /F 2>$null
schtasks /Delete /TN "\LinkSwitch\ApplyAuto"     /F 2>$null
schtasks /Delete /TN "\LinkSwitch\Restore"       /F 2>$null
Get-NetIPInterface | Sort AddressFamily,InterfaceMetric |
  Format-Table AddressFamily,ifIndex,InterfaceAlias,InterfaceMetric,AutomaticMetric,ConnectionState -Auto
```

**Capture a baseline first** and commit it to the repo as `docs/baseline-<host>.txt`:
```powershell
Get-NetIPInterface | Export-Csv baseline-ipinterface.csv
Get-NetRoute -DestinationPrefix '0.0.0.0/0','::/0' | Export-Csv baseline-routes.csv
Get-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WcmSvc\GroupPolicy' -EA SilentlyContinue |
  Out-File baseline-wcm.txt
```

### Ordered test gates

| # | Test | Elevation | Mutates? | Pass criterion | Rollback |
|---|---|---|---|---|---|
| **T0** | Scaffold builds. `cargo build` with the full feature list; `cargo tree -e normal --prefix none \| sed 's/ (\*)//' \| awk 'NF' \| sort -u \| wc -l` ≈ 96. Widget launches, idle 2-min Task Manager soak. | no | no | Clean build; ~0% idle CPU. | n/a |
| **T1** | **Transparency + shape.** Borderless + transparent + always-on-top under glow, on the real GPU. Visually confirm: not black, rounded corners, drag works, no taskbar button, no Alt-Tab entry. | no | no | Looks right. | Switch to `wgpu`, or drop transparency. **Decide here — do not proceed on an unverified renderer.** |
| **T2** | **Read layer.** `--status` prints adapters, hardware/connector flags, kind, per-family metrics, default routes with totals, verdict, WCM effective policy. Cross-check against `Get-NetIPInterface`, `netsh interface ipv4 show interfaces`, `Get-NetRoute`. | no | no | Exactly two hardware candidates (ifIndex 4, 14); metrics match `netsh` (use netsh, **not** `Get-NetIPInterface`, as the oracle — it renders ProtonVPN's metric as `$null` where the API and netsh both say 0). | n/a |
| **T3** | **The premise test. Plug the Ethernet cable in.** Observe with `--status` on a 10-s loop for 5 minutes: does Wi-Fi stay associated? Does it auto-connect after a reboot with the cable in? Then run `netsh wlan connect name="<SSID>"` manually and re-observe for 5 minutes. | no | Wi-Fi assoc only | Establishes whether a *manual* connect survives at `fMinimizeConnections = 1` — the one genuinely open behavioural question. | Nothing to undo. |
| **T4** | **The elevation gate.** Register one throwaway task (`\LinkSwitchTest\Probe`, `HighestAvailable`, `InteractiveToken`, action = `linkswitch.exe --apply auto`). Trigger it from an **unelevated** shell via `schtasks /Run`. | install elevated; trigger unelevated | metrics → automatic (already the state) | `LastTaskResult = 0`, log shows `SetIpInterfaceEntry` returning `NO_ERROR`, **no UAC prompt**. If it returns 5, R2's fallback is required — find out now. | `schtasks /Delete /TN "\LinkSwitchTest\Probe" /F` |
| **T5** | **First real write.** With both links up, `--apply wifi`. | via task | **yes** | Ethernet v4 metric = 9000; `SitePrefixLength` trap did not fire (no code 87); v6 reports `FamilyAbsent` cleanly; Ethernet `MediaConnectState` still Connected, `OperStatus` still Up, IP unchanged. `Find-NetRoute -RemoteIPAddress 1.1.1.1` shows the Wi-Fi source address. Browse to an IP-echo site — public IP changes (VPN off). | `--apply auto`, then `restore.ps1`. |
| **T6** | **Back and forth.** `--apply ethernet`, `--apply wifi`, `--apply auto`, 3 cycles. Diff the whole `MIB_IPINTERFACE_ROW` before/after to confirm the read-modify-write did not silently pin `NlMtu` or anything else. | via task | yes | Deterministic, idempotent, no drift in any field but `Metric`/`UseAutomaticMetric`. Latency from click to verdict-flip < 1.5 s. | `--apply auto`. |
| **T7** | **Persistence.** `--apply wifi`, then reboot with the `Restore` task **disabled**. Check `Get-NetIPInterface` at logon. Then re-enable `Restore` and reboot again. | n/a | yes (survives reboot) | Answers the store question definitively. Either way `Restore` makes the outcome correct — record which it was in the README. | `restore.ps1`. |
| **T8** | **Cable replug + resume.** With `--apply wifi` active: unplug, wait 30 s, replug. Then sleep/resume. | n/a | yes | Ethernet metric still 9000 (or `Restore` re-applies it). If it resets on replug, add the `EventTrigger` from §5.3. | `--apply auto`. |
| **T9** | **VPN interaction.** Connect ProtonVPN, then switch modes. | n/a | yes | Widget shows `Hijacked` with the correct name; the flip still succeeds at the interface level; the VPN's `/32` server host-route stays where it was; nothing crashes. | Disconnect VPN, `--apply auto`. |
| **T10** | **WCM opt-in.** Enable the checkbox, confirm the registry write, confirm `WcmQueryProperty` reports `(0, 1)`, reboot, verify Wi-Fi auto-connects with the cable in. Then uninstall and verify the value is **deleted** (it was originally absent), not set to 1. | elevated | **machine policy** | Backup/restore is byte-exact. | `Remove-ItemProperty ... fMinimizeConnections`. |
| **T11** | **Full install/uninstall round trip** on a clean VM. Install → switch → uninstall → diff `Get-NetIPInterface` and the registry against the T0 baseline. | elevated | yes | Zero residue: no tasks, no `%ProgramFiles%`/`%ProgramData%` folders, every interface back to `AutomaticMetric Enabled`, WCM value as it was. | n/a |

**Standing rules during testing:**
- Every test from T4 on runs with `restore.ps1` one keystroke away in an elevated window.
- Never test with only one link up — the failure modes (WCM soft-disconnect, automatic-metric buckets, missing default route) only appear with both live.
- Re-capture `Get-NetIPInterface` before **and** after each mutating test and diff; drift in any field other than `InterfaceMetric`/`AutomaticMetric` is a bug in the read-modify-write.
- T4 gates all UI polish. If `HighestAvailable` alone does not satisfy `SetIpInterfaceEntry`, the binary layout changes, and you want to know that on day one, not day ten.