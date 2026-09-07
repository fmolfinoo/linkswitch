# LinkSwitch — SPEC: Mode 3 "Wi‑Fi only / zero Ethernet"

**Status:** implementable. Targets the existing tree at `C:\dev\linkswitch` (edition 2021, rust-version 1.82, `windows = "0.62"`, existing `Mode{Ethernet,Wifi,Auto}`, existing `Journal`/`MachineConfig` in `src/config.rs`, existing elevated worker in `src/apply.rs`, existing task registration in `src/tasks.rs`).
**Every claim below that contradicts an earlier research dive reflects the adversarial verification, which wins.**

---

## 1. DECISION

### 1.1 The ruling

| | Mechanism | Ship as |
|---|---|---|
| **(a)** | **SOFT OFF** — unbind `ms_tcpip` + `ms_tcpip6` from the Ethernet adapter via INetCfg COM | **DEFAULT** |
| **(b)** | **HARD OFF** — disable the adapter devnode via SetupAPI `DIF_PROPERTYCHANGE` | Explicit opt-in, labelled **"Strict"** |

### 1.2 Does "cut the IP stack, link stays up" genuinely achieve zero Ethernet networking?

**No. It achieves zero *IP*. It does not achieve zero *networking*. Only device-disable achieves literal zero.**

State that plainly in the UI, the README and the release notes. The honest decomposition:

**What SOFT OFF provably removes** (verified on the dev box using `ms_tcpip6`, which is already unbound on Ethernet/Wi‑Fi/VMnet1/VMnet8, against `vEthernet (Default Switch)` as the bound control):

* No `MSFT_NetIPInterface` row for that family — the CIM query *throws* `No matching objects found`, it does not return empty.
* Zero routes of any kind: no default route, no on-link subnet route, no `/32`/`/128` host route, no `224.0.0.0/4`, no `255.255.255.255/32`, no `ff00::/8`, no `fe80::/64`.
* Zero unicast addresses, including link-local/APIPA.
* Zero DNS servers (`ServerAddresses = {}`), not stale ones.
* Absent from `netsh interface ipv4|ipv6 show interfaces`.
* Empty ARP/ND cache.
* No DHCP DISCOVER/renew — no interface in the stack means no client activity.
* No NLA/flyout entry for that family; with both families unbound the network drops out of Network List Manager entirely.
* NetBIOS, SMB server, SMB client die **transitively and automatically** — `NetBT\Linkage\Bind = \Device\Tcpip_{GUID}`, `LanmanServer|LanmanWorkstation\Linkage\Bind = \Device\NetBT_Tcpip_{GUID}`. None of them bind to `\Device\{GUID}` directly. **Do not touch `ms_netbt`, `ms_netbios`, `ms_server`, `ms_msclient`.**

**The residual — what still reaches the wire under SOFT OFF.** Authoritative source is the NIC's own `Linkage\UpperBind`, read live on the dev machine (Intel I225‑V):

```
lltdio  MsLldp  Ndisuio  RasPppoe  RDMANDK  rspndr  Tcpip  VMnetBridge
```

Remove `Tcpip` and seven protocol drivers still have a live path to the copper:

| Residual | What it does | Severity |
|---|---|---|
| **`MsLldp`** (LLDP, EtherType `0x88CC`) | Service Running/Automatic. Keeps advertising this machine to switches. | Discoverability |
| **`lltdio` + `rspndr`** (LLTD, `0x88D9` → `01-00-5E-00-00-0E`) | MS‑LLTD is *explicitly IP-independent*. `rspndr` **actively answers** neighbours' network-map probes. The PC stays visible and mappable from any other Windows box on that wired LAN. | Discoverability / privacy |
| **`VMnetBridge` (`vmware_bridge`) / `ms_l2bridge` / `vms_pp`** | A bridged VM puts **its own MAC and its own IP** on that wire at layer 2, entirely below the host stack. Unbinding host TCP/IP does nothing to it. `vmware_bridge=True` and `ms_l2bridge=True` on the dev machine **right now**. | **Correctness hole — "zero Ethernet" is factually false on such a box** |
| **`Ndisuio`** | 802.1X EAPOL (`0x888E`) path. Idle by default (`dot3svc` is Stopped/Manual), but live if the user ever enabled the adapter's Authentication tab. | Conditional |
| **`RasPppoe`** | `0x8863/0x8864`. Idle unless a PPPoE dialer exists. | Dormant |
| **`ms_ndiscap`** | Bound and enabled. ETW capture provider (`netsh trace`, `pktmon`) still sees every frame. | Observability only |
| **`RDMANDK`** | No traffic on a consumer NIC. | None |

**Therefore:**

* SOFT OFF = **zero IP presence, non-zero layer‑2 emission.**
* HARD OFF = **zero frames of any kind**, but the port goes dark and Wake‑on‑LAN dies.
* Physically unplugging = zero frames, but *non*-zero stale local artifacts (measured on the dev box with the cable out and `ms_tcpip` bound: 2 routes, a Tentative APIPA address `169.254.10.20`, a Permanent neighbor entry `224.0.0.22 → 01-00-5E-00-00-16`, a stale DNS server `192.168.1.1` persisted as `DhcpNameServer`, and a row in `netsh`).

**Do not ship the line "soft off is cleaner than unplugging."** It is a category error. Unplugging emits zero frames; soft-off with the link up does not. The truthful framing is: *soft off is cleaner inside Windows, unplugging is cleaner on the wire.*

### 1.3 Why SOFT OFF is nonetheless the default

1. It is the only mechanism that satisfies the user's actual stated requirement — *the cable stays plugged in and the NIC keeps working as hardware* — while removing every route, address and DNS server.
2. It is reversible in ~1 s with no devnode churn: the LUID, the interface index, the adapter GUID and every per-interface config key (`Services\Tcpip[6]\Parameters\Interfaces\{guid}`) survive untouched. **Unbinding is a bind-list edit, not a config wipe** — verified: 17 IPv6 interface-config keys exist against only 4 entries in `Tcpip6\Linkage\Bind`, and the unbound Ethernet key still holds all 16 `Dhcpv6*` values.
3. HARD OFF **persists across reboots** as `CONFIGFLAG_DISABLED` and kills Wake‑on‑LAN. A Wi‑Fi driver crash after a reboot leaves a machine with zero connectivity and possibly no LinkSwitch. That is a worse default.
4. HARD OFF on a NIC carrying a VMware bridge or an external Hyper‑V vSwitch takes every VM offline and can leave the vSwitch degraded (documented analogue: NIC Teaming, where Enable does not undo Disable because the virtual adapter was destroyed and recreated disabled).

### 1.4 Two claims that must NOT be shipped

* **"PoE keeps working."** Meaningless. Power over Ethernet is delivered *by* PSE *to* a powered device. A desktop NIC is not a PD. Delete every mention.
* **"The switch port stays lit / the link never drops."** **Unverified, and Microsoft's own `Disable-NetAdapterBinding` documentation says the operation "restarts the network adapter."** A miniport restart typically resets the PHY. The earlier evidence for "link stays up with both protocols unbound" (WAN Miniport (IP), Hyper‑V vSwitch Extension Adapter) was invalid — `IsBindableTo` returns *no* for both, i.e. those components have **no tcpip binding path at all**; they are unbindable, not unbound, and they are `NCF_VIRTUAL|NCF_HIDDEN` pseudo-devices with no PHY.
  **Gate G-LINK (blocking, pre-1.0):** on real hardware, unbind `ms_tcpip`+`ms_tcpip6` on a NIC plugged into a managed switch; watch the switch's port-link counter and poll `Get-NetAdapter | select Status,MediaConnectionState` at 100 ms across the whole apply cycle. Until G‑LINK passes, the UI says **"the cable stays plugged in and the adapter stays enabled"** and nothing stronger.
  **Gate G‑WOL (blocking, pre-1.0):** send a real magic packet to the soft-off adapter. The dev box has `*WakeOnMagicPacket=Enabled`, `*WakeOnPattern=Enabled`, `WakeOnMagicPacketFromS5=Enabled`. Ship the claim only as **"magic-packet Wake-on-LAN"** — wake-on-ARP/pattern uses `*PMARPOffload`/`*PMNSOffload`, which the IP stack registers *together with the interface's IP address*; with no address there is nothing to offload, so pattern wake degrades even if magic-packet wake survives.

---

## 2. DEFAULT MECHANISM — implementation path

### 2.1 Decision: native **INetCfg COM**, in-process in the elevated worker. Not WMI. Not PowerShell.

**Justification from verified evidence:**

* Everything is generated. `windows 0.62.2` `Win32::NetworkManagement::NetManagement` contains `INetCfg, INetCfgLock, INetCfgComponent, INetCfgComponentBindings, INetCfgBindingPath, INetCfgBindingInterface, INetCfgClass, IEnumNetCfgComponent, IEnumNetCfgBindingPath, IEnumNetCfgBindingInterface` and all `NETCFG_*` HRESULTs, behind **one** Cargo feature: `Win32_NetworkManagement_NetManagement`. No `windows-bindgen`, no `#[interface]`, no third-party crate. (No crate wraps INetCfg — `netcfg`/`inetcfg`/`netadapter` on crates.io are all unrelated.)
* The whole COM chain was executed live on Win11 25H2 (10.0.26200) **from a non-elevated process**: `CoCreateInstance(CLSID_CNetCfg)` → QI `INetCfg` → QI `INetCfgLock` → `IsWriteLocked` → `Initialize` → `FindComponent` → `EnumComponents(GUID_DEVCLASS_NET)` → `IsBoundTo` → `Uninitialize`, all `S_OK`, results matching `Get-NetAdapterBinding` 1:1. The stale forum claim of `REGDB_E_CLASSNOTREG` does not reproduce; `InprocServer32` = `C:\Windows\System32\NetSetupShim.dll`, `ThreadingModel = Both`, registered in both 64- and 32-bit views.
* **The decisive functional advantage:** `AcquireWriteLock`'s `ppszwClientDescription` out-param **names the process holding the netcfg write lock**. The single most common real-world failure is the user having an adapter's Properties dialog open in `ncpa.cpl`, which holds that lock for as long as it is on screen (as do Device Manager network operations, Hyper‑V vSwitch create/delete, VPN installers, `netsh`/`netcfg` installs). WMI and PowerShell acquire and release the lock internally with a timeout you cannot set or observe, so they can only report a generic failure. LinkSwitch can say *"Windows network configuration is locked by X — close its window and retry."*
* WMI (`ROOT/StandardCimv2:MSFT_NetAdapterBindingSettingData`, key `"{guid}::ms_tcpip"`, methods `Enable()`/`Disable()`) is **more** code than INetCfg once you plumb `IWbemClassObject` in/out params, gives `uint32` return codes instead of `NETCFG_*` HRESULTs, and hides the lock. It is **not** a way around the lock: `NetAdapterCim.dll` imports `NetSetupApi.dll` (an RPC client into `NetSetupSvc`), and `NetSetupShim.dll` — same family — *is* the InprocServer32 of `CLSID_CNetCfg`. All three routes converge on one engine and one system-wide lock.
* PowerShell measured **~1 s of pure overhead per click before any work** (cold spawn + `Get-NetAdapterBinding`: 982/918/931 ms on re-measure; bare `powershell.exe -NoProfile 'exit 0'`: 142/169/135 ms), plus console-flash risk and localized error text. `pwsh` is **not** installed on the dev machine — any shell-out must target `powershell.exe` (WinPS 5.1).
* `netcfg.exe` cannot do this. Its verbs are component install/uninstall (`-c p|s|c -i <id>`), `-q`, `-u`, `-s`, `-b` (**read-only** — prints binding paths), `-m` (dumps `NetworkBindingMap.txt`), `-d`, `-x`. There is no per-adapter bind/unbind verb. **`netcfg -d` and `netcfg -x` perform a cleanup pass over ALL networking devices and force a reboot.** If the elevated worker ever invokes `netcfg.exe` (only ever for `-b`/`-m` diagnostics), hardcode the full argv; never interpolate.

**PowerShell survives only in two places:** the README rescue script (§7 — correct tool there: inbox, needs no binary) and a hidden `config.json` escape hatch `"binding_backend": "powershell"` for field debugging. It is never the default path.

### 2.2 Cargo.toml — merge into the existing `[dependencies.windows]` table

**Do not add a second `[dependencies]`/`[dependencies.windows]` block — duplicate key is a TOML parse error.** Add exactly four features to the existing list:

```toml
  "Win32_NetworkManagement_NetManagement",       # ALL INetCfg* + NETCFG_* HRESULTs  (soft off)
  "Win32_Devices_DeviceAndDriverInstallation",   # SetupDi*, CM_*, GUID_DEVCLASS_NET (hard off)
  "Win32_Networking_NetworkListManager",         # INetworkListManager, NLM_CONNECTIVITY
  "Win32_System_RemoteDesktop",                  # WTSQuerySessionInformationW (RDP guardrail)
```

Already present and required: `Win32_Foundation`, `Win32_System_Com`, `Win32_System_Registry`, `Win32_NetworkManagement_IpHelper`, **`Win32_NetworkManagement_Ndis`** (without it `GetAdaptersAddresses` and `IP_ADAPTER_ADDRESSES_LH` do not exist at all — the struct embeds `Ndis::IF_OPER_STATUS`), `Win32_Networking_WinSock`, `Win32_UI_WindowsAndMessaging`.

`Win32_Foundation` is redundant (`Win32_NetworkManagement_NetManagement → Win32_NetworkManagement → Win32 → Win32_Foundation`) — harmless, leave it.

**Pin `windows = "0.62"`.** 0.62.2 is the latest published (2025‑10‑06). windows-rs master is 0.100.0, edition 2024, with the entire `Win32_*` feature namespace **removed**. Do not chase master.

**Stay on edition 2021.** The generated wrappers are `pub unsafe fn`; under edition 2024 `unsafe_op_in_unsafe_fn` fires (9 warnings on the helper module alone) and a CI with `-D warnings` hard-fails.

### 2.3 New module `src/net/binding.rs`

#### Constants windows-rs does not generate

```rust
use windows::core::{w, GUID, PCWSTR};

/// netcfgx.h. Verified absent from the whole crate (0 hits for "CNetCfg"/"5b035261").
pub const CLSID_CNETCFG: GUID = GUID::from_u128(0x5b035261_40f9_11d1_aaec_00805fc1270e);

/// Also at Win32::Devices::DeviceAndDriverInstallation::GUID_DEVCLASS_NET; local copy is cheaper.
pub const GUID_DEVCLASS_NET: GUID = GUID::from_u128(0x4d36e972_e325_11ce_bfc1_08002be10318);

/// FindComponent matching is case-insensitive. NETCFG_TRANS_CID_MS_TCPIP exists in the crate;
/// there is NO ms_tcpip6 constant anywhere. Hardcode both for symmetry.
pub const CID_MS_TCPIP:  PCWSTR = w!("MS_TCPIP");
pub const CID_MS_TCPIP6: PCWSTR = w!("MS_TCPIP6");

/// NCF_* are COMPONENT_CHARACTERISTICS(i32) newtypes; `chars & NCF_PHYSICAL` is a compile error.
pub const NCF_PHYSICAL_U32: u32 = 4;
```

#### The three load-bearing traps

**Trap 1 — `.ok()` destroys `S_FALSE`.** The generated `INetCfgLock::AcquireWriteLock` calls `.ok()` on the HRESULT. Microsoft documents `S_FALSE` as *"the wait time elapsed before the OS granted the lock"* — i.e. **failure to acquire**. `windows-result`'s `is_ok()` is literally `self.0 >= 0`, so `S_FALSE (1)` maps to `Ok(())`. You believe you hold the lock, then eat `NETCFG_E_NO_WRITE_LOCK (0x8004A024)` from `UnbindFrom`/`Apply`. Reproduced in one line on the dev box: raw `IsWriteLocked` returned `0x00000001` while `lock.IsWriteLocked(None).is_ok()` returned `true`.

**The same bug destroys the *answer* of:** `INetCfgComponentBindings::IsBoundTo`, `IsBindableTo`, `SupportsBindingInterface`, `INetCfgLock::IsWriteLocked`, `INetCfgBindingPath::IsEnabled`, and — **omitted from the original dive** — **`INetCfg::FindComponent`**, which returns `S_FALSE` for an unknown component id (verified: `FindComponent("ms_bogus_xyz") → 0x00000001`). Every one must go through the raw vtable.

**Trap 2 — `IsBoundTo` direction.** Call it on the **PROTOCOL**, passing the **ADAPTER**: `tcpip_bindings.IsBoundTo(adapter)`. The intuitive reverse compiles, runs, and returns `S_FALSE` for **every** adapter on the machine including provably-bound ones — the QI for `INetCfgComponentBindings` on the adapter *succeeds*, so there is no error to warn you. Verified in both directions in one run: protocol→adapter reproduces `Get-NetAdapterBinding` 1:1; adapter→protocol is `S_FALSE` for all 22.

**Trap 3 — lock ordering.** `AcquireWriteLock` may only be called **before** `INetCfg::Initialize` or **after** `Uninitialize`. Calling it while initialized returns `NETCFG_E_ALREADY_INITIALIZED (0x8004A020)`.

#### Raw-vtable helpers (mandatory)

```rust
use windows::core::{Interface, Error, HRESULT, PWSTR, Result};
use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::NetworkManagement::NetManagement::{
    INetCfg, INetCfgComponent, INetCfgComponentBindings, INetCfgLock,
};

pub enum LockOutcome { Acquired, Busy { holder: Option<String> } }

unsafe fn take_pwstr(p: PWSTR) -> Option<String> {
    if p.is_null() { return None; }
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.as_ptr() as *const core::ffi::c_void)); // CoTaskMemAlloc'd — must free
    s
}

pub unsafe fn acquire_write_lock(lock: &INetCfgLock, ms: u32, client: PCWSTR) -> Result<LockOutcome> {
    let mut holder = PWSTR::null();
    let hr: HRESULT = (Interface::vtable(lock).AcquireWriteLock)(
        Interface::as_raw(lock), ms, client, &mut holder as *mut PWSTR);
    let who = take_pwstr(holder);
    match hr {
        S_OK    => Ok(LockOutcome::Acquired),
        S_FALSE => Ok(LockOutcome::Busy { holder: who }),   // <- surface `who` in the UI
        e       => Err(e.into()),                            // E_ACCESSDENIED = not admin
    }
}

/// PROTOCOL's bindings interface, ADAPTER as the argument. Never the reverse.
pub unsafe fn proto_is_bound_to(b: &INetCfgComponentBindings, adapter: &INetCfgComponent) -> Result<bool> {
    let hr = (Interface::vtable(b).IsBoundTo)(Interface::as_raw(b), Interface::as_raw(adapter));
    if hr.is_err() { return Err(hr.into()); }
    Ok(hr == S_OK)
}

/// S_FALSE => component is not installed on this machine. That is DATA, not an error.
pub unsafe fn find_component(cfg: &INetCfg, id: PCWSTR) -> Result<Option<INetCfgComponent>> {
    let mut out: Option<INetCfgComponent> = None;
    let hr = (Interface::vtable(cfg).FindComponent)(
        Interface::as_raw(cfg), id, &mut out as *mut _ as *mut *mut core::ffi::c_void);
    if hr.is_err() { return Err(hr.into()); }
    Ok(if hr == S_OK { out } else { None })
}
```

#### RAII guards — **mandatory; the naive `?` version leaks the machine-wide netcfg lock**

`Cargo.toml` deliberately does **not** set `panic = "abort"` (documented reason: an abort produces `0xC0000409` with zero log lines). So unwind safety is on us.

```rust
struct ComGuard { uninit: bool }
impl ComGuard {
    fn enter() -> Result<Self> {
        // CLSID_CNetCfg ThreadingModel = Both. Use STA: netcfg notify objects can raise UI, and
        // MS's own sample uses CoInitialize(NULL). RPC_E_CHANGED_MODE means the thread was already
        // MTA -- proceed, and do NOT CoUninitialize.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr == RPC_E_CHANGED_MODE { return Ok(Self { uninit: false }); }
        hr.ok()?;                       // S_OK and S_FALSE both balance with CoUninitialize
        Ok(Self { uninit: true })
    }
}
impl Drop for ComGuard { fn drop(&mut self) { if self.uninit { unsafe { CoUninitialize() } } } }

struct LockGuard<'a>(&'a INetCfgLock);
impl Drop for LockGuard<'_> { fn drop(&mut self) { unsafe { let _ = self.0.ReleaseWriteLock(); } } }

struct InitGuard<'a> { cfg: &'a INetCfg, committed: bool }
impl Drop for InitGuard<'_> {
    fn drop(&mut self) { unsafe { if !self.committed { let _ = self.cfg.Cancel(); }
                                  let _ = self.cfg.Uninitialize(); } }
}
```

**Declare `LockGuard` first, `InitGuard` second.** Rust drops in reverse declaration order, which reproduces the documented teardown exactly: `Cancel?` → `Uninitialize` → `ReleaseWriteLock` → Release → `CoUninitialize`.

#### The operation

```rust
/// `want` = (bind_v4, bind_v6). On RESTORE pass the snapshotted tuple. NEVER (true, true).
/// Returns Ok(needs_reboot).
pub fn set_tcpip_binding(adapter_guid: GUID, want: (bool, bool)) -> Result<bool> {
    let _com = ComGuard::enter()?;
    let netcfg: INetCfg = unsafe { CoCreateInstance(&CLSID_CNETCFG, None, CLSCTX_INPROC_SERVER)? };
    let lock: INetCfgLock = netcfg.cast()?;

    // 1. LOCK FIRST -- before Initialize, or NETCFG_E_ALREADY_INITIALIZED.
    //    5 s, one retry after 2 s. Report the holder's name verbatim.
    match unsafe { acquire_write_lock(&lock, 5_000, w!("LinkSwitch"))? } {
        LockOutcome::Acquired => {}
        LockOutcome::Busy { .. } => {
            std::thread::sleep(Duration::from_secs(2));
            match unsafe { acquire_write_lock(&lock, 5_000, w!("LinkSwitch"))? } {
                LockOutcome::Acquired => {}
                LockOutcome::Busy { holder } => return Err(Error::new(
                    NETCFG_E_NO_WRITE_LOCK,
                    format!("Windows network configuration is locked by {}",
                            holder.as_deref().unwrap_or("another application")))),
            }
        }
    }
    let _lock_guard = LockGuard(&lock);

    // 2. Initialize.
    unsafe { netcfg.Initialize(None)? };
    let mut init = InitGuard { cfg: &netcfg, committed: false };

    // 3. Resolve protocols. None => component not installed => nothing to unbind for that family.
    let v4 = unsafe { find_component(&netcfg, CID_MS_TCPIP)?  };
    let v6 = unsafe { find_component(&netcfg, CID_MS_TCPIP6)? };
    if v4.is_none() && want.0 { return Err(/* IPv4 protocol missing -- refuse, do not claim success */); }

    // 4. Resolve the adapter by GetInstanceGuid == Get-NetAdapter InterfaceGuid.
    let adapter = unsafe { find_adapter(&netcfg, adapter_guid) }?;   // NETCFG_E_ADAPTER_NOT_FOUND

    // 5. Bind / unbind. Bindings interface comes from the PROTOCOL.
    let mut changed = false;
    for (proto, want_bound) in [(v4.as_ref(), want.0), (v6.as_ref(), want.1)] {
        let Some(p) = proto else { continue };
        let b: INetCfgComponentBindings = p.cast()?;
        if unsafe { proto_is_bound_to(&b, &adapter)? } == want_bound { continue; }
        unsafe { if want_bound { b.BindTo(&adapter)? } else { b.UnbindFrom(&adapter)? } };
        changed = true;
    }

    // 6. Commit. Apply's HRESULT must be read RAW: NETCFG_S_REBOOT (0x0004A020) is a SUCCESS code,
    //    so `?` swallows it and you report success on a half-applied change.
    //    Documented on the INetCfg::Apply page: S_OK, NETCFG_S_REBOOT, or NETCFG_E_NO_WRITE_LOCK.
    let mut needs_reboot = false;
    if changed {
        let hr = unsafe { (Interface::vtable(&netcfg).Apply)(Interface::as_raw(&netcfg)) };
        if hr.is_err() { return Err(hr.into()); }        // InitGuard Cancels + Uninitializes
        needs_reboot = hr == NETCFG_S_REBOOT;
    }
    init.committed = true;
    Ok(needs_reboot)
}

unsafe fn find_adapter(cfg: &INetCfg, want: GUID) -> Result<INetCfgComponent> {
    let mut en: Option<IEnumNetCfgComponent> = None;
    cfg.EnumComponents(&GUID_DEVCLASS_NET, Some(&mut en))?;
    let Some(en) = en else { return Err(E_UNEXPECTED.into()) };   // never unwrap() under the lock
    loop {
        let mut raw = core::ptr::null_mut();
        let mut n = 0u32;
        if (Interface::vtable(&en).Next)(Interface::as_raw(&en), 1, &mut raw, &mut n) != S_OK
           || n == 0 || raw.is_null() { break; }
        let c = INetCfgComponent::from_raw(raw);
        let mut g = GUID::zeroed();
        if c.GetInstanceGuid(Some(&mut g)).is_ok() && g == want { return Ok(c); }
    }
    Err(NETCFG_E_ADAPTER_NOT_FOUND.into())
}
```

Wrap the whole call in `std::panic::catch_unwind(AssertUnwindSafe(..))` in `apply.rs`; the guards release the lock during unwind, and the catch lets the worker journal the failure and exit with a distinct code instead of dying silently.

#### HRESULTs to map to user-facing text

`NETCFG_E_NO_WRITE_LOCK 0x8004A024` · `NETCFG_E_ALREADY_INITIALIZED 0x8004A020` · `NETCFG_E_NOT_INITIALIZED 0x8004A021` · `NETCFG_E_IN_USE 0x8004A022` · `NETCFG_E_NEED_REBOOT 0x8004A025` · `NETCFG_E_ACTIVE_RAS_CONNECTIONS 0x8004A026` · `NETCFG_E_ADAPTER_NOT_FOUND 0x8004A027` · **`NETCFG_E_VMSWITCH_ACTIVE_OVER_ADAPTER 0x8004A02A`** (real failure mode — Hyper‑V external vSwitch over the NIC) · `NETCFG_S_REBOOT 0x0004A020`.

#### Post-apply obligations

1. **Re-read every binding on the adapter and diff against the pre-change snapshot.** Microsoft documents the side effect verbatim: *"Disabling some adapter bindings can automatically enable other network adapter bindings."* It is documented behaviour, not a theoretical risk. Any unexpected delta goes in the log and the journal's `last_error`.
2. **Poll for the IP interface to actually disappear** (both families, up to 10 s) via the existing `metric::snapshot()` / `GetIpInterfaceEntry`. The `Apply()` return says nothing about NDIS teardown, route flush, or DHCP state.
3. If `needs_reboot`, journal it and tell the user the change is **staged**, not applied.

### 2.4 Reading binding state (unprivileged — the widget can do it)

Read-only INetCfg needs **no elevation** (verified from a non-elevated process). But for the widget's fast refresh loop, use the cheap registry read and gate it on device presence:

* `HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Linkage` value `Bind` (REG_MULTI_SZ) contains `\Device\{adapterGUID}` **iff** `ms_tcpip` is bound. Same for `Tcpip6`. Verified both directions: `Tcpip6\Linkage\Bind` = exactly the four adapters reporting `ms_tcpip6=True`.
* **Gate on presence first.** `Tcpip\Linkage\Bind` carries 14 GUIDs on the dev box including devices that are not present at all (Apple Mobile Device Ethernet, two UsbNcm, Wintun) which `Get-NetAdapterBinding -IncludeHidden` does not even list. Intersect with `adapters::enumerate()`.
* Authority (used before/after every apply and when the mode‑3 panel opens) is the INetCfg read path.

**Never match adapters by display name.** `INetCfgComponent::GetDisplayName()` on an adapter returns the hardware description (`"Intel(R) Ethernet Controller (3) I225-V"`), not the connection alias (`"Ethernet"`). The alias lives at `HKLM\SYSTEM\CurrentControlSet\Control\Network\{4D36E972-...}\{ifGuid}\Connection\Name`. Match on `GetInstanceGuid` == `Nic.adapter_name`.

**INetCfg's adapter set ≠ `Get-NetAdapter`'s, in both directions.** On the dev box both return 22, differing by 4 each way: INetCfg sees three absent devnodes (`GetDeviceStatus` → `Err(NETCFG_E_ADAPTER_NOT_FOUND)`, which is the cheap phantom filter — treat the Err as data, do not `?` it), and misses `vEthernet (Default Switch)`, Teredo, IP‑HTTPS and 6to4 — which are **exactly** the four entries in `Tcpip6\Linkage\Bind`. Therefore: **drive the adapter picker from the existing `adapters::enumerate()`**, resolve into INetCfg by instance GUID, and treat `NETCFG_E_ADAPTER_NOT_FOUND` as a first-class user-visible error ("this adapter cannot be configured by LinkSwitch"), never a generic failure.

---

## 3. SECONDARY MECHANISM — HARD OFF

### 3.1 Path: SetupAPI `DIF_PROPERTYCHANGE` via **`SetupDiCallClassInstaller`**

**Not `SetupDiChangeState`.** Microsoft: *"Only a class installer should call SetupDiChangeState."* It is the *default handler* the class installer chains to; calling it directly bypasses the network class installer and co-installers, so the netcfg-side teardown never runs. Device Manager and `devcon` use `SetupDiCallClassInstaller`.

### 3.2 Sequence

1. `SetupDiGetClassDevsW(Some(&GUID_DEVCLASS_NET), PCWSTR::null(), None, DIGCF_PRESENT)` → wrap in `windows::core::Owned<HDEVINFO>` (HDEVINFO implements `Free`) so destroy is panic-safe. **Keep `DIGCF_PRESENT`** — a disabled devnode is still *present*. If an enable reports not-found, retry **without** `DIGCF_PRESENT` to distinguish "disabled" from "physically gone".
2. Loop `SetupDiEnumDeviceInfo` with `SP_DEVINFO_DATA.cbSize` set each iteration. Terminate on `e.code() == ERROR_NO_MORE_ITEMS.to_hresult()`.
3. Per candidate: `SetupDiOpenDevRegKey(hdev, &data, DICS_FLAG_GLOBAL.0, 0, DIREG_DRV, KEY_READ.0)` (all four params are plain `u32`, so `.0` is correct) → `RegQueryValueExW("NetCfgInstanceId")` → compare against the target braced GUID. This is OpenVPN's production technique (`src/tapctl/tap.c :: get_net_adapter_guid`). **Never match on `SPDRP_FRIENDLYNAME`/`SPDRP_DEVICEDESC`** — the dev box has *"Microsoft Wi-Fi Direct Virtual Adapter"* twice and *"UsbNcm Host Device"* twice. `let _ = RegCloseKey(hkey);` — `WIN32_ERROR` is `#[must_use]`.
4. **Before mutating:** `SetupDiGetDeviceInstallParamsW` and bail if `DI_DONOTCALLCONFIGMG` is set — Microsoft: *"you should not call SetupDiChangeState for the device but should instead set the DI_NEEDREBOOT flag."*
5. Build `SP_PROPCHANGE_PARAMS`, call `SetupDiSetClassInstallParamsW` then `SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, ...)`.
6. `SetupDiGetDeviceInstallParamsW` again; `Flags & (DI_NEEDREBOOT|DI_NEEDRESTART)` → journal `needs_reboot`.

### 3.3 Scope flags — mirror `devcon`, do **not** use GLOBAL-only

`devcon`'s `ControlCallback` (Windows-driver-samples `setup/devcon/cmds.cpp`) issues `DIF_PROPERTYCHANGE` **twice** for enable and uses CONFIGSPECIFIC for disable:

* **DISABLE:** one pass, `Scope = DICS_FLAG_CONFIGSPECIFIC`, `HwProfile = 0`.
* **ENABLE:** pass 1 `Scope = DICS_FLAG_GLOBAL` (tolerate failure), then pass 2 `Scope = DICS_FLAG_CONFIGSPECIFIC`, `HwProfile = 0`.

Concrete failure of GLOBAL-only enable: a NIC the user disabled from Device Manager (config-specific) is not brought back, and LinkSwitch looks broken on the exact path users reach for when they panic.

### 3.4 The four `cbSize` traps and the two type traps

```rust
let mut p = SP_PROPCHANGE_PARAMS::default();
p.ClassInstallHeader.cbSize = size_of::<SP_CLASSINSTALL_HEADER>() as u32;  // header size
p.ClassInstallHeader.InstallFunction = DIF_PROPERTYCHANGE;                  // DI_FUNCTION(18)
p.StateChange = DICS_DISABLE;                     // SETUP_DI_STATE_CHANGE
p.Scope       = DICS_FLAG_CONFIGSPECIFIC;         // SETUP_DI_PROPERTY_CHANGE_SCOPE
p.HwProfile   = 0;

// SP_DEVINFO_DATA is a documented [in,out] param -- SetupDiChangeState may update DevInst.
// Passing `&devinfo` (a shared borrow) to a callee that writes through it is UB and discards
// the refreshed DevInst. Derive from a mutable place:
let dip = core::ptr::addr_of_mut!(devinfo) as *const SP_DEVINFO_DATA;

unsafe {
    SetupDiSetClassInstallParamsW(
        hdev, Some(dip),
        Some(&p.ClassInstallHeader as *const SP_CLASSINSTALL_HEADER),
        size_of::<SP_PROPCHANGE_PARAMS>() as u32,   // WHOLE struct, NOT the header
    )?;
    SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, hdev, Some(dip))?;
}
```

Fourth `cbSize`: `SP_DEVINSTALL_PARAMS_W.cbSize` before both `SetupDiGetDeviceInstallParamsW` calls.

**`windows::core::Error::from_win32()` does not exist** in windows 0.62.2 / windows-result 0.4.1. The candidates are `empty, new, from_hresult, from_thread`. `GetAdaptersAddresses`/`GetExtendedTcpTable` *return* the code and do not set thread-last-error, so `from_thread` is also semantically wrong. Use:
```rust
Err(windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(rc)))
```

### 3.5 Persistence, identity and readiness

* Persistent global-disable bit is `CONFIGFLAG_DISABLED (0x1)` in **`HKLM\SYSTEM\CurrentControlSet\Enum\<InstanceId>\ConfigFlags`** — the *hardware* key (`DIREG_DEV` / `SPDRP_CONFIGFLAGS`), **not** the Class software key that holds `NetCfgInstanceId`. Verified: no class subkey `0000..0021` has a `ConfigFlags` value; the Enum key for the I225‑V does. Never write it directly.
* Disabled shows as `CM_PROB_DISABLED` / Device Manager **Code 22**. Correct detection:
  ```rust
  let mut status  = CM_DEVNODE_STATUS_FLAGS::default();   // NOT u32
  let mut problem = CM_PROB::default();                    // NOT u32
  let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
  cr == CR_SUCCESS && (status.0 & DN_HAS_PROBLEM.0) != 0 && problem == CM_PROB_DISABLED
  ```
  **Never branch on `Win32_NetworkAdapter.NetEnabled`.** On the dev box, cable merely unplugged: `NetEnabled=False`, `NetConnectionStatus=7`, `ConfigManagerErrorCode=0`, `Get-PnpDevice` → `CM_PROB_NONE`. Branching on `NetEnabled` makes the app read the user's cable-unplug as its own hard-off.
* **`ifIndex` and `LUID` change across disable/enable** (`IP_ADAPTER_ADDRESSES_LH` Remarks: *"may change when an adapter is disabled and then enabled"*). Persist and re-resolve on **PnP device instance ID** first (`SetupDiGetDeviceInstanceIdW`), then permanent MAC, then hardware ID. `AdapterRef` gains `device_instance_id`. Note MAC alone is insufficient — the dev box has Wi‑Fi `aa-bb-cc-dd-ee-02` and Bluetooth PAN `aa-bb-cc-dd-ee-03`.
* **`SetupDiCallClassInstaller` returning is not networking readiness.** NDIS teardown, TCP/IP unbind, route flush, media sense and DHCP DISCOVER are all still in flight. Poll `GetIfTable2`/`MIB_IF_ROW2.OperStatus` (works with no IP binding) until it settles. Show an indeterminate spinner, never a countdown — no first-party timing figures exist for NIC enable/disable.
* SetupAPI's own error codes (`ERROR_NO_SUCH_DEVINST = 0xE0000203`) get masked by `HRESULT::from_win32` into `0x80070203`, indistinguishable from Win32 515. Round-tripping compares equal so matching works; recover the raw code with `WIN32_ERROR::from_error(&e)` before logging, and never print `e.message()` expecting a SetupAPI string.
* USB NIC surprise-removal between enumeration and the call → re-resolve the whole match, never retry the stale `SP_DEVINFO_DATA`.
* `SetupDiCallClassInstaller` → `ERROR_ACCESS_DENIED (5)` means **not elevated**, i.e. the scheduled-task registration is broken (wrong RunLevel/user context). Treat as a canary, not a retryable condition.
* No Windows-10 version floor is introduced by any `SetupDi*` call (`SetupDiOpenDevRegKey` is *"Available in Microsoft Windows 2000 and later"*; the 14393 string on that page is the API-set row, irrelevant because windows-rs links `setupapi.dll` directly). **The real floor is in the README rescue text:** `pnputil /disable-device` and `/enable-device` require **Windows 10 2004+**.

---

## 4. THE THREE-MODE STATE MACHINE

### 4.1 Modes

`Mode` in `src/config.rs` gains one variant. `SCHEMA` → `2`.

```rust
pub enum Mode { Ethernet, Wifi, WifiOnly, Auto }
```
`as_str()` → `"wifi-only"`; `parse()` accepts `"wifi-only" | "wifionly" | "wifi_only" | "only"`; `task_name()` → `"ApplyWifiOnly"`. Register the task in `tasks::ALL_TASKS` alongside the existing three, same `TASK_LOGON_INTERACTIVE_TOKEN` / RunLevel HIGHEST shape. `cli.rs` gains `--apply wifi-only` and, for `wifi-only` only, an optional `--force` (documented as CLI-only; the GUI never sets it).

| Mode | Ethernet | Wi‑Fi | Mechanism |
|---|---|---|---|
| **Ethernet** | wins the default route | standby | interface metric (existing) |
| **Wi‑Fi** | stays up, keeps its own LAN subnet + a *worse* default route (so it still fails over) | wins | interface metric, Ethernet parked at `park_metric` (default 9000, clamped 100–9999) |
| **Wi‑Fi only** | **no IP interface at all** | wins by default | Wi‑Fi mode's metric work **+** `set_tcpip_binding(eth, (false,false))` |
| **Auto** | automatic metric | automatic metric | existing |

**Mode 2 keeps metric steering, not `DisableDefaultRoutes`.** `MIB_IPINTERFACE_ROW.DisableDefaultRoutes` (`Set-NetIPInterface -IgnoreDefaultRoutes`) is the exact, documented way to strip a default route while keeping on-link routes — but it removes graceful failover, which is the whole point of mode 2 ("Ethernet stays as a backup"). Expose it as an advanced sub-option **"Strict Wi‑Fi (no Ethernet fallback)"**, default off. If ever enabled it **must** be journalled per family and reset by uninstall and by the rescue script — the naive rescue script does not reset it.

**Mode 3 does not touch Ethernet's metric.** Once `ms_tcpip` is unbound there is no IPv4 interface, so `SetIpInterfaceEntry` returns `ERROR_NOT_FOUND` — which `src/net/err.rs` already treats as success-not-failure. Mode 3 records and restores **bindings only** for Ethernet; the metric journal for Ethernet is written and restored by the mode‑1/2/auto paths as today.

### 4.2 Transitions

Notation: `J←` = journal write, atomic (`.tmp` + rename), **before** the operation it precedes.

---

#### T1 — `* → WifiOnly` (entering mode 3)

```
 1. Elevation check.                                    -> exit NOT_ELEVATED (2)
 2. Recover a torn journal from a previous crash (§5.5). Blocks until clean.
 3. Resolve Ethernet + Wi-Fi (config::resolve -> LUID, MAC, name, desc, device_instance_id).
                                                        -> exit NOT_CONFIGURED (3)
 4. GUARDRAILS (§5.4), in this order:
       G1 last-link quorum   G2 RDP self-lockout   G3 VPN over Ethernet
       G4 layer-2 bridge     G5 not already in a torn state
    Any hard refusal -> exit BLOCKED_GUARDRAIL (11), nothing written, nothing changed.
 5. PRE-FLIGHT Wi-Fi (§5.1). Associate if needed (existing wifi:: path, 15 s).
    Must reach: OperStatus Up, radio on, associated, non-link-local Preferred unicast address,
    a gateway, and NLM per-connection IPV4_INTERNET|IPV6_INTERNET for the Wi-Fi adapter GUID.
                                                        -> exit WIFI_UNAVAILABLE (4)
 6. Snapshot BOTH protocols' bound state on Ethernet via the INetCfg read path.
       (v4_bound, v6_bound)  <- e.g. (true, false) on the dev box: IPv6 is ALREADY off.
    Snapshot every OTHER binding on the adapter too, for the post-apply auto-enable diff.
 7. J<- { schema:2, mode:"wifi-only", in_flight:true, started_unix, commit_deadline_unix:+20,
          restore:[Ethernet+Wi-Fi metric backups],
          restore_bindings:[{luid, adapter_guid, device_instance_id,
                             ms_tcpip: <prior>, ms_tcpip6: <prior>, strict_extra:[...] }],
          touched: <union, never pruned> }
 8. Register the WATCHDOG task (§5.3) -- BEFORE any mutation.
 9. Apply the Wi-Fi mode metric work (unchanged existing code path).
10. set_tcpip_binding(eth_guid, (false, false)).
    [Strict soft off only] then unbind ms_lldp, ms_lltdio, ms_rspndr.
11. Poll up to 10 s until Ethernet has zero IP interfaces in BOTH families.
12. Re-read ALL Ethernet bindings; diff vs step 6; any unexpected auto-enable -> log + last_error.
13. WATCHDOG verify (§5.3): settle 3 s, then poll NLM every 1 s to commit_deadline;
    require 2 consecutive positives whose verdict timestamp is AFTER step 10.
        success -> J<- { in_flight:false, finished_unix, last_error:null }; delete watchdog task.
        failure -> auto-revert T4; J<- { in_flight:false, last_error:"..." }; exit VERIFY_FAILED (6).
14. If Apply returned NETCFG_S_REBOOT: journal needs_reboot, exit REBOOT_REQUIRED (10),
    UI says "staged", NOT "done".
```

---

#### T2 — `WifiOnly → Wifi`

```
1..3 as T1 (elevation, torn-journal recovery, resolve).
4.  J<- { mode:"wifi", in_flight:true, restore_bindings:<the CURRENT (false,false) state>,
          restore:[metric backups], commit_deadline_unix:+20 }.
5.  Register watchdog task.
6.  set_tcpip_binding(eth_guid, journal.restore_bindings.prior)   <- the SNAPSHOTTED tuple,
        e.g. (true, false) on the dev box. NEVER (true, true).
    [Strict] rebind ms_lldp/ms_lltdio/ms_rspndr to their snapshotted values.
7.  WAIT for the interface to come back: poll until both wanted families have a Preferred
    non-link-local address OR 20 s elapse. Metric writes before this fail with ERROR_NOT_FOUND.
    DHCP DISCOVER goes out immediately on rebind; a v4 lease normally lands in ~1-3 s.
8.  NOW apply the mode-2 metric work.
9.  Verify + commit as T1 step 13. Delete watchdog.
```

---

#### T3 — `WifiOnly → Ethernet`

Identical to T2 with the mode‑1 metric work at step 8. **The rebind must complete and the interface must reappear before Ethernet can be made to win** — attempting to lower a metric on a nonexistent interface is a silent no-op that would leave the user on Wi‑Fi while the UI claims Ethernet.

---

#### T4 — auto-revert (watchdog failure or crash recovery)

```
1. Read the journal. Treat every field as UNTRUSTED input (§5.2).
2. Re-resolve each recorded adapter: device_instance_id -> MAC -> name -> description.
   Refuse to act on any id that does not resolve to a present GUID_DEVCLASS_NET device.
3. For each restore_devices entry:  enable (GLOBAL then CONFIGSPECIFIC) if it was enabled before.
4. For each restore_bindings entry: set_tcpip_binding(guid, (prior_v4, prior_v6)).
5. Wait for interfaces (as T2 step 7).
6. For each restore metric backup: automatic ? UseAutomaticMetric=TRUE : (FALSE + Metric).
7. Reset any IgnoreDefaultRoutes we set.
8. J<- { in_flight:false, last_error:"auto-reverted: <reason>" }. Delete the watchdog task.
9. Toast the user: what happened, and that the machine was put back.
```

---

#### T5 — `WifiOnly (Strict / HARD OFF)`

Same as T1 with step 10 replaced by the SetupAPI disable, and additionally journalling `restore_devices:[{device_instance_id, was_enabled:true}]`. **HARD OFF requires an explicit extra confirmation in the UI** naming the two consequences: the port goes dark and Wake‑on‑LAN stops.

---

## 5. THE SAFETY SYSTEM

### 5.1 Pre-flight (all read-only; runs before anything is written)

**Adapter probe — one `GetAdaptersAddresses` call, with the right flags:**

```rust
let flags = GET_ADAPTERS_ADDRESSES_FLAGS(
    GAA_FLAG_SKIP_ANYCAST.0 | GAA_FLAG_SKIP_MULTICAST.0 | GAA_FLAG_INCLUDE_GATEWAYS.0);
```

* **`GAA_FLAG_INCLUDE_GATEWAYS` (0x0080) is mandatory.** Without it `FirstGatewayAddress` is **always null** and the pre-flight gate can never pass — mode 3 would be permanently unreachable. Proven on the dev box: the live internet-carrying Wi‑Fi adapter reported `gateway=false` without the flag and `gateway=true` with it.
* **Do not set `GAA_FLAG_SKIP_DNS_SERVER`** — mode 3's own acceptance criterion is "Ethernet contributes zero DNS servers", which needs `FirstDnsServerAddress`.
* **`!FirstUnicastAddress.is_null()` is not "has an IP."** A link-local/APIPA address is a unicast address. Measured: the physically-*disconnected* Ethernet has a non-null `FirstUnicastAddress` holding `169.254.10.20` (`PrefixOrigin=WellKnown, SuffixOrigin=Link, AddressState=Tentative`). A Wi‑Fi card that associates but fails DHCP is identical. **Require at least one address that is not `169.254.0.0/16`, not `fe80::/10`, and whose `IP_ADAPTER_UNICAST_ADDRESS_LH.DadState == IpDadStatePreferred`.**
* MS strongly discourages the grow-the-buffer loop; pre-allocate 15 KB.
* `use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;` — it is **not** in the IpHelper glob. `use windows::Win32::Foundation::HANDLE;` for the WLAN client handle — the WiFi glob does not export it, and rustc will unhelpfully suggest the wrong `std::os::windows::raw::HANDLE`.

**Wi‑Fi radio/association:** `WlanQueryInterface(..., wlan_intf_opcode_interface_state, ...)` must equal `wlan_interface_state_connected` before `wlan_intf_opcode_current_connection` is queryable (it returns `ERROR_INVALID_STATE` otherwise). Free every out-buffer with `WlanFreeMemory`. Existing `src/net/wifi.rs` already owns this.

**Internet verdict — per-connection, not system-wide:**

```rust
// windows-rs DOES emit the coclass GUID as a constant: Win32::Networking::NetworkListManager::NetworkListManager
// (verified equal to DCB00C01-570F-4A9B-8D69-199FDBA5723B). Do not hand-declare it.
let nlm: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL)?;
// Enumerate GetNetworkConnections(); match INetworkConnection::GetAdapterId() against the
// target adapter's GUID; read THAT connection's GetConnectivity().
let ok = (c.0 & NLM_CONNECTIVITY_IPV4_INTERNET.0) != 0 || (c.0 & NLM_CONNECTIVITY_IPV6_INTERNET.0) != 0;
```

* Verified live: adapterId `66666666-…` = Wi‑Fi, connectivity `0x42`; adapterId `EEEEEEEE-…` = ProtonVPN, connectivity `0x42`. Per-connection works — **use it.**
* **Do not** use the "demote Ethernet first, then the system-wide verdict is the Wi‑Fi verdict" shortcut. It is false whenever a VPN or virtual adapter is present: the dev box has **two** `0.0.0.0/0` routes and the ProtonVPN tunnel (RouteMetric 0, *no* interface metric at all) wins — all 102 established TCP connections have `localAddr 10.0.0.2`.
* `IsConnectedToInternet` returns `VARIANT_BOOL`; `VARIANT_TRUE` is **-1**. Test `.0 != 0`, never `.0 == 1`.
* `NLM_CONNECTIVITY`: `DISCONNECTED 0`, `IPV4_NOTRAFFIC 0x1`, `IPV6_NOTRAFFIC 0x2`, `IPV4_SUBNET 0x10`, `IPV4_LOCALNETWORK 0x20`, `IPV4_INTERNET 0x40`, `IPV6_SUBNET 0x100`, `IPV6_LOCALNETWORK 0x200`, `IPV6_INTERNET 0x400`.
* **Captive portal reads as no-IPv4-internet. That is correct behaviour** — refuse to switch into a portal the user has not signed into. Do not "work around" it.
* **NCSI trustworthiness:** read `HKLM\SYSTEM\CurrentControlSet\Services\NlaSvc\Parameters\Internet` — on the dev box `EnableActiveProbing=1`, `ActiveWebProbeHost=www.msftconnecttest.com`, `ActiveWebProbePath=connecttest.txt`, `ActiveWebProbeContent="Microsoft Connect Test"`, `ActiveDnsProbeHost=dns.msftncsi.com`, `ActiveDnsProbeContent=131.107.255.255`. If `EnableActiveProbing=0`, tell the user NLM's verdict is unreliable on this machine and fall back to the **machine's own configured** DNS probe (resolve `ActiveDnsProbeHost`, require exactly `ActiveDnsProbeContent`) — never invent endpoints.

**COM lifetime.** Every COM helper uses the `ComGuard` from §2.3. `RPC_E_CHANGED_MODE` must be handled (proceed, do not uninitialize) or the check hard-fails on any thread another library already put in the MTA. Drop the interface **before** `CoUninitialize` — an explicit `drop(nlm);` or an inner scope.

**Metric read/write** (already in `src/net/metric.rs`; two things to confirm):
* `UseAutomaticMetric = TRUE` **means the "Automatic metric" box is CHECKED**, i.e. it is the *default*. Steering is `FALSE` + explicit `Metric`. Getting this inverted in the restore path is the single worst possible bug.
* **For IPv4, `SitePrefixLength` must be set to 0 before every `SetIpInterfaceEntry`** — a naive Get→mutate→Set round trip fails with `ERROR_INVALID_PARAMETER`, inside the rollback path.
* Rows are **per address family**. One `metric: u32` cannot represent both; the existing `MetricBackup{luid, family_v6, metric, automatic}` already gets this right.

### 5.2 State journal — schema v2

Path `%ProgramData%\LinkSwitch\state.json`, atomic write, elevated-worker-only writer.

```jsonc
{
  "schema": 2,
  "mode": "wifi-only",                    // ethernet | wifi | wifi-only | auto
  "in_flight": true,                      // Pending. Set BEFORE the first mutation.
  "started_unix": 1788800000,
  "commit_deadline_unix": 1788800020,     // started + watchdog window
  "finished_unix": null,
  "needs_reboot": false,                  // Apply() returned NETCFG_S_REBOOT / DI_NEEDREBOOT
  "last_error": null,

  // --- v1 field, unchanged ---
  "restore": [
    { "luid": 1234567890, "family_v6": false, "metric": 25, "automatic": true },
    { "luid": 1234567890, "family_v6": true,  "metric": 25, "automatic": true }
  ],

  // --- v2: soft off ---
  "restore_bindings": [
    {
      "luid": 1234567890,
      "adapter_guid": "{11111111-2222-3333-4444-555555555555}",
      "device_instance_id": "PCI\\VEN_8086&DEV_15F3&SUBSYS_87D21043&REV_03\\C87F54FFFF0711AA00",
      "mac": "aa:bb:cc:dd:ee:ff",
      "ms_tcpip":  true,                  // PRIOR value. On this dev box ms_tcpip6 is ALREADY false.
      "ms_tcpip6": false,
      "strict_extra": [                   // only populated in Strict soft off
        { "component_id": "ms_lldp",   "enabled": true },
        { "component_id": "ms_lltdio", "enabled": true },
        { "component_id": "ms_rspndr", "enabled": true }
      ],
      "all_bindings_before": { "ms_pacer": true, "vmware_bridge": true, "ms_l2bridge": true }
    }
  ],

  // --- v2: hard off ---
  "restore_devices": [
    { "device_instance_id": "PCI\\VEN_8086&DEV_15F3&...", "mac": "aa:bb:cc:dd:ee:ff",
      "was_enabled": true, "scope": "configspecific" }
  ],

  // --- v2: strict Wi-Fi sub-option ---
  "restore_default_routes": [
    { "luid": 1234567890, "family_v6": false, "disable_default_routes": false }
  ],

  // --- v2: NEVER pruned except by uninstall. Union of everything ever touched. ---
  "touched": [
    { "luid": 1234567890,
      "adapter_guid": "{11111111-...}",
      "device_instance_id": "PCI\\VEN_8086&DEV_15F3&...",
      "mac": "aa:bb:cc:dd:ee:ff",
      "name": "Ethernet",
      "description": "Intel(R) Ethernet Controller (3) I225-V",
      "first_touched_unix": 1788700000 }
  ]
}
```

**Serde rules:** every v2 field `#[serde(default)]`, so a v1 journal loads. `load_json` already degrades a corrupt file to `Default` rather than panicking — keep that, but **log it loudly**: it means recovery information was lost.

`is_torn()` extends to `in_flight && (!restore.is_empty() || !restore_bindings.is_empty() || !restore_devices.is_empty())`.

**The restore rule is already in the codebase and is non-negotiable:** *LinkSwitch never restores a value it did not capture, and never writes a hardcoded "default" in place of one.* This is not fussiness — on the dev machine a VPN has applied IPv6 leak protection by unbinding `ms_tcpip6` from **nine** adapters (ProtonVPN, Wi‑Fi, Ethernet, Bluetooth, VMnet1, VMnet8, Kernel Debugger, and two `Local Area Connection*`). A restore that "helpfully" rebound IPv6 would leak the user's real address.

**Journal integrity — the elevated worker treats the file as hostile input.**

`C:\ProgramData`'s default DACL grants `BUILTIN\Users` Write with ContainerInherit and `CREATOR OWNER` Generic All inherit-only. `C:\ProgramData\LinkSwitch` does not exist yet on the dev box, i.e. the squat is available right now. Combined with a RunLevel HIGHEST, no-UAC task that replays the journal at boot, an unprivileged user could hand an auto-elevating process an attacker-chosen `device_instance_id` for `CM_Disable_DevNode`. `install.rs::harden_dacl` already runs `icacls /inheritance:r /grant:r Administrators:F SYSTEM:F Users:RX /T`. Three additions:

1. **Take ownership first**, before the grant: `icacls <dir> /setowner *S-1-5-32-544 /T /C /Q`. An owner retains implicit `WRITE_DAC` and can undo the grant otherwise.
2. **Harden before the first journal write**, not after (already the case — verify it stays that way).
3. **Verify on every read** in the worker: owner is `S-1-5-32-544` or `S-1-5-18`, and the DACL is protected. Refuse to act and log loudly otherwise. Independently, **re-resolve every `device_instance_id` and `adapter_guid` from the journal against a live `SetupDiGetClassDevsW(GUID_DEVCLASS_NET)` enumeration** and refuse anything that is not a present network-class device — never pass a journal string straight to `CM_*`.

### 5.3 Watchdog and auto-revert

**Window `N = 20 s`.** Wi‑Fi already passed a full internet pre-flight, so the window only needs to confirm it *still* holds after the change; 20 s covers a slow DHCP renew without leaving the user stranded for a minute.

**Verification loop:** settle **3 s**, then poll every 1 s until `commit_deadline_unix`. Require **2 consecutive positives** whose NLM verdict timestamp is **after** the mutation timestamp. **NLM's verdict is event-driven and cached** — a bare 1 s poll starting immediately after the mutation is likely to read the *pre-change* answer and commit a broken state, which is the exact failure the watchdog exists to prevent. Prefer registering `INetworkListManagerEvents`/`INetworkConnectionEvents` via `IConnectionPointContainer` (or `NotifyIpInterfaceChange`) and requiring a change notification with a post-apply timestamp; the poll is the fallback.

**The crash the boot/logon triggers do not cover.** "At log on" and "At system startup" do **not** fire for *"the worker died three seconds after mutating, and the machine kept running"* — which is precisely when the user is stranded and cannot reach a browser to look up the fix.

**Fix:** a fifth scheduled task, `Watchdog`, in the `\LinkSwitch\` folder:

* Registered **before** the mutation (T1 step 8), deleted on commit.
* Trigger: one-time, `now + 45 s`, **repeat every 1 minute for 15 minutes**.
* Action: `linkswitch.exe --recover`.
* RunLevel HIGHEST, `TASK_LOGON_INTERACTIVE_TOKEN`.
* **`StartWhenAvailable=true`; "Start only if a network connection is available" MUST be off** (it defaults off — the installer must not set it). **"Run only on AC power" must be off** or a laptop on battery never recovers.

`--recover` is idempotent: journal not torn → no-op, exit 0.

**Auto-revert triggers:** deadline reached without 2 fresh positives · the Wi‑Fi connection object disappears · `set_tcpip_binding` returns any error after a partial change · `--recover` finds `in_flight == true`.

### 5.4 Guardrails

**G1 — last-link quorum (hard refuse).** Refuse mode 3 unless at least one **non-tunnel** adapter other than Ethernet currently has an IPv4 (or IPv6) default route with a live next hop **and** a per-connection NLM `*_INTERNET` verdict. **Exclude tunnels from the quorum** — `IP_ADAPTER_ADDRESSES_LH.IfType == IF_TYPE_TUNNEL (131)`, `TunnelType != TUNNEL_TYPE_NONE`, or a `0.0.0.0/0` route on a non-physical ifIndex. A VPN riding over Ethernet is not an independent path; taking Ethernet dark takes the tunnel with it, and a stale tunnel connection object can still report "internet OK" to the watchdog while the user is actually stranded. Use `GetBestRoute2` to a public address to answer "who really carries traffic", not a metric comparison.

**G2 — RDP self-lockout (hard refuse; `--force` overrides on the CLI only).**

```
a) Session check:
     GetSystemMetrics(SM_REMOTESESSION) != 0
   OR ProcessIdToSessionId(GetCurrentProcessId())
        != HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\GlassSessionId   (=1 on dev box)
   -- the second form is required because SM_REMOTESESSION under-detects with RemoteFX vGPU.
b) Port:  read HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp\PortNumber
          (3389 on the dev box, but it is machine-configurable). Do NOT hardcode.
c) Table: GetExtendedTcpTable(TCP_TABLE_OWNER_PID_CONNECTIONS) for BOTH AF_INET
          (MIB_TCPTABLE_OWNER_PID / MIB_TCPROW_OWNER_PID, dwLocalAddr: u32)
          AND AF_INET6 (MIB_TCP6TABLE_OWNER_PID / MIB_TCP6ROW_OWNER_PID, ucLocalAddr: [u8;16]).
d) Match: dwState == MIB_TCP_STATE_ESTAB (5)
       && decoded local port == the configured port
       && local address is one of the Ethernet adapter's unicast addresses.
```
Port decoding, settled empirically against the live table: `u16::from_be((row.dwLocalPort & 0xFFFF) as u16)` — confirmed by independently decoding remote port 443 on HTTPS rows. **Drop the `.to_le()`** from the earlier snippet; it is a no-op on x86‑64, reads as if it did something, and breaks on any port.

**G2b — informational, not blocking.** Count established TCP connections bound to any Ethernet unicast address and show *"N open connections on Ethernet will drop."*

**G3 — VPN over Ethernet (warn + confirm).** If a tunnel adapter holds the default route and its underlying transport is Ethernet, mode 3 kills the tunnel. Say so before proceeding.

**G4 — layer-2 bridge (warn + offer Strict).** If any of `vmware_bridge`, `ms_l2bridge`, `vms_pp`, `ms_implat` is Enabled on the target adapter, SOFT OFF does not achieve zero Ethernet: bridged VMs keep putting their own MAC and IP on that wire. Both `vmware_bridge` and `ms_l2bridge` are **True on the dev box today**. Show the banner from §8 and offer HARD OFF. **Never silently unbind `vmware_bridge`** — that breaks the user's bridged VMs.

**G5 — torn journal.** Refuse any new apply while `is_torn()`; run recovery first.

**Component allow/deny lists — build from an enumerated snapshot, never a hardcoded array.** The dev box's Ethernet has **21** bindings; a machine with Intel PROSet, teaming or third-party filters will have IDs nobody has seen. Baseline policy:

* **UNBIND (default soft off):** `ms_tcpip`, `ms_tcpip6` — nothing else.
* **UNBIND (Strict soft off, opt-in):** additionally `ms_lldp`, `ms_lltdio`, `ms_rspndr`. Cost: this PC no longer appears on neighbours' Windows network map. No functional loss.
* **NEVER TOUCH:** `ms_pacer`, `ms_wfplwf_upper`, `ms_wfplwf_lower` (**these are the Windows Filtering Platform / firewall attachment points** — removing them silently unfilters the adapter, the exact opposite of the safety goal), `ms_ndiscap`, `ms_ndisuio`, `ms_rdma_ndk`, `ms_pppoe`, `ms_server`, `ms_msclient`, `ms_netbt`, `ms_netbios`, **`ms_l1vhlwf`** (Nested Network Virtualization — Enabled and missing from every earlier list).
* **DETECT AND WARN, never auto-unbind:** `vmware_bridge`, `ms_l2bridge`, `vms_pp`, `ms_implat`.

**Do NOT ship `*NdisDeviceType = 1`**, in any mode, even as a last resort. It hides the adapter from the flyout and Network and Sharing Center, but: Microsoft scopes it to endpoint devices and says you must not set it on anything providing external connectivity (which Ethernet is, in modes 1 and 2); it is a driver-key value with no per-mode scope, so it degrades NLA in modes 1 and 2 too; it requires an adapter restart, which drops the link — defeating the one property mode 3 exists to preserve; and driver updates rewrite the class key from the INF and can silently drop it.

### 5.5 Crash recovery

`--recover` runs at three points: the `Recover` task's logon trigger, an added **system-startup** trigger, and the `Watchdog` task's repeating trigger. Order:

1. Verify elevation, verify journal-directory ownership/DACL.
2. Load the journal; if unparsable, log loudly and exit 1 (a truncated file means recovery info was lost — do not guess).
3. `is_torn()` false → exit 0.
4. `is_torn()` true → **treat Pending as failure unconditionally, with no network check.** An unwitnessed change is never trusted. Execute T4.
5. Toast on next widget start: *"LinkSwitch put your network back after an interrupted change."*

**Does the netcfg write lock survive a worker crash?** Expected to be released on process death (the lock is held by `NetSetupSvc` on behalf of a client) but **unverified**. The `LockGuard`/`InitGuard` + `catch_unwind` design makes it survivable in every case except a hard kill. If `--recover` finds `IsWriteLocked` still asserted with LinkSwitch as the holder, log it and surface *"restart the machine to clear a stuck Windows network lock."*

### 5.6 Drift detection

* **Model `IntendedState` separately from a computed `ActualState`.** Never assume a past `Apply` is still in effect.
* **Fast path (widget refresh loop, unprivileged):** registry `Services\Tcpip[6]\Linkage\Bind` membership, **gated on device presence** from the existing `adapters::enumerate()` (the bind list contains GUIDs for absent devices).
* **Authority (before/after every apply, and when the mode-3 panel opens):** the INetCfg read path — which needs no elevation.
* **Triggers:** app launch, `WM_DEVICECHANGE`/`DBT_DEVNODES_CHANGED`, resume from sleep, and the existing `net::notify` change callbacks. **Not a periodic silent re-apply.**
* **UI policy:** show a banner — *"LinkSwitch's last change was undone outside the app"* — with a one-click **Reapply**. Never silently reapply: the user's manual change may have been deliberate.
* **Drift causes to expect:** Device Manager / `netsh` / another tool run by hand; a driver upgrade; a vendor installer (Intel PROSet, VMware Workstation upgrade, Hyper‑V feature enable/disable) re-running netcfg; and **the documented auto-enable side effect of disabling a binding**.
* **Persistence limits.** The unbind lives in `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e972-...}\<NNNN>\Linkage\UpperBind` plus `Services\Tcpip[6]\Linkage\Bind` — a persistent hive, so it survives reboot, sleep/resume and dock replug. A driver *upgrade* that keeps the devnode keeps it. **A device uninstall-and-reinstall recreates the key from the INF with all protocols bound.** Windows feature updates migrate these keys with no documented guarantee. Hence: re-read real state on every launch, never trust the stored mode.
* **"Network reset" (`netcfg -d`) reverts both mechanisms and mints a NEW adapter GUID**, so `adapter_guid` alone goes stale. The existing `config::resolve` LUID→MAC→name→description chain already covers this; extend it with `device_instance_id` as the first fallback after LUID, and note MAC alone is ambiguous on combo Wi‑Fi/BT modules.
* Class-key subkey numbering is **positional and unstable** (`0000`=I225‑V, `0010`=Wi‑Fi, `0020/0021`=VMnet on this box only). Always locate the class subkey by scanning for `NetCfgInstanceId == adapter GUID` (~22 keys, cheap), and skip the non-device `Configuration` subkey.

---

## 6. UNINSTALL / RESTORE

`--uninstall`, elevated. Order matters.

```
 1. Verify elevation; verify journal dir ownership + protected DACL.
 2. Load the journal. If is_torn(), run T4 recovery FIRST and wait for it to settle.
 3. For every entry in `touched` (the never-pruned union, NOT just `restore`):
      a. Re-resolve: device_instance_id -> MAC -> LUID -> name -> description.
         Unresolvable -> log "adapter <name> is no longer present; nothing to restore" and continue.
      b. restore_devices:  enable if was_enabled  (GLOBAL pass, tolerate failure; then CONFIGSPECIFIC).
                           Wait for OperStatus to settle via GetIfTable2.
      c. restore_bindings: set_tcpip_binding(guid, (ms_tcpip, ms_tcpip6)) -- the RECORDED prior
                           tuple. Then strict_extra, each to its recorded value.
                           NEVER a hardcoded (true, true).
      d. Wait for the IP interfaces to reappear (up to 20 s) before touching metrics.
      e. restore:          automatic ? UseAutomaticMetric=TRUE : (FALSE + Metric).
                           IPv4 rows: SitePrefixLength = 0 first.
      f. restore_default_routes: reset DisableDefaultRoutes to the recorded value.
 4. WCM policy: existing WcmBackup semantics -- if `present == false`, DELETE the value;
    do not write 0 back, which would leave the policy explicitly disabled rather than at default.
 5. DNS policy (if the mode-2 hardening was ever enabled): same present/value semantics on
    HKLM\SOFTWARE\Policies\Microsoft\Windows NT\DNSClient\DisableSmartNameResolution.
 6. Remove the per-user autostart Run value.
 7. tasks::remove_all() -- ApplyEthernet, ApplyWifi, ApplyWifiOnly, ApplyAuto, Recover, Watchdog,
    then the \LinkSwitch\ folder.
 8. Write a final summary line to worker.log; copy it to %TEMP%\linkswitch-uninstall.log
    (ProgramData is about to vanish).
 9. Delete %ProgramData%\LinkSwitch (config.json, state.json, logs\).
10. Delete %LOCALAPPDATA%\LinkSwitch (prefs.json, widget.log).
11. Print, per adapter, exactly what was put back. Exit 0 only if every step succeeded;
    on partial failure exit 1 and print the README rescue section's location.
```

**Uninstall must never write a Windows default it did not capture.** If `touched` is empty, uninstall touches no network state at all — it only removes tasks and files.

---

## 7. README — rescue section (copy-paste)

> ### If your network is broken and LinkSwitch can't fix it
>
> This script undoes everything LinkSwitch can do. It works even if LinkSwitch has been
> uninstalled or deleted. It needs **no internet connection**.
>
> 1. Press **Start**, type **PowerShell**, right-click **Windows PowerShell**, choose
>    **Run as administrator**.
> 2. Paste the whole block below and press Enter.
> 3. It **shows you what it will change and asks once** before changing anything.

```powershell
#requires -RunAsAdministrator
# LinkSwitch emergency restore. Shows a plan, asks once, then restores.
# Add -Force to skip the prompt.  Add -All to include adapters LinkSwitch never touched.
param([switch]$Force, [switch]$All)
$ErrorActionPreference = 'Stop'

Write-Host "=== LinkSwitch emergency restore ===" -ForegroundColor Cyan

# Prefer LinkSwitch's own record of what it touched; fall back to physical Ethernet/Wi-Fi.
$state = "$env:ProgramData\LinkSwitch\state.json"
$targets = @()
if (Test-Path $state) {
    try {
        $j = Get-Content $state -Raw | ConvertFrom-Json
        foreach ($t in @($j.touched)) { $targets += $t.mac }
        Write-Host "Found LinkSwitch's record: $($targets.Count) adapter(s) it touched."
    } catch { Write-Warning "state.json is unreadable; falling back to all physical adapters." }
}

# InterfaceType 6 = Ethernet, 71 = 802.11. Never blanket-touch tunnels or virtual switches.
$nics = Get-NetAdapter -IncludeHidden | Where-Object { $_.InterfaceType -in 6,71 }
if (-not $All -and $targets.Count -gt 0) {
    $norm = $targets | ForEach-Object { ($_ -replace '[:-]','').ToUpper() }
    $nics = $nics | Where-Object { $norm -contains ($_.MacAddress -replace '[:-]','').ToUpper() }
}
if (-not $nics) { Write-Warning "No matching adapters. Re-run with -All."; return }

Write-Host "`nPlan:" -ForegroundColor Yellow
foreach ($n in $nics) {
    Write-Host ("  {0}  [{1}]  status={2}" -f $n.Name, $n.InterfaceDescription, $n.Status)
    if ($n.Status -eq 'Disabled') { Write-Host "      - enable the adapter" }
    Get-NetAdapterBinding -Name $n.Name -ComponentID ms_tcpip,ms_tcpip6 -ErrorAction SilentlyContinue |
        Where-Object { -not $_.Enabled } |
        ForEach-Object { Write-Host "      - re-enable binding $($_.ComponentID)" }
    Write-Host "      - set IPv4/IPv6 interface metric back to automatic"
    Write-Host "      - stop ignoring Ethernet/Wi-Fi default routes"
}
Write-Host ""
Write-Host "NOTE: this re-enables IPv4 AND IPv6. If a VPN deliberately turned IPv6 off," -ForegroundColor Yellow
Write-Host "      turn it back off in that VPN's settings afterwards." -ForegroundColor Yellow

if (-not $Force) {
    if ((Read-Host "`nProceed? (y/N)") -notmatch '^(y|yes)$') { Write-Host "Cancelled."; return }
}

foreach ($n in $nics) {
    Write-Host "`n--- $($n.Name) ---" -ForegroundColor Cyan
    if ($n.Status -eq 'Disabled') {
        Write-Host "Enabling adapter..."
        Enable-NetAdapter -Name $n.Name -Confirm:$false
        Start-Sleep -Seconds 3    # Enable-NetAdapter is asynchronous.
    }
    foreach ($cid in 'ms_tcpip','ms_tcpip6') {
        $b = Get-NetAdapterBinding -Name $n.Name -ComponentID $cid -ErrorAction SilentlyContinue
        if ($b -and -not $b.Enabled) {
            Write-Host "Re-enabling $cid ..."
            Enable-NetAdapterBinding -Name $n.Name -ComponentID $cid -Confirm:$false
        }
    }
    Start-Sleep -Seconds 2    # let the IP interface come back before touching it
    foreach ($fam in 'IPv4','IPv6') {
        $i = Get-NetIPInterface -InterfaceAlias $n.Name -AddressFamily $fam -ErrorAction SilentlyContinue
        if ($i) {
            Write-Host "Restoring $fam automatic metric and default routes..."
            Set-NetIPInterface -InterfaceAlias $n.Name -AddressFamily $fam `
                               -AutomaticMetric Enabled -IgnoreDefaultRoutes Disabled
        } else {
            Write-Host "$fam has no interface on this adapter (nothing to restore)."
        }
    }
}

Write-Host "`n=== Done. Current state: ===" -ForegroundColor Green
Get-NetAdapter -IncludeHidden | Where-Object InterfaceType -in 6,71 |
    Format-Table Name,Status,LinkSpeed -AutoSize
Get-NetRoute -DestinationPrefix 0.0.0.0/0 | Format-Table ifIndex,NextHop,RouteMetric -AutoSize
Write-Host "If you still have no network, reboot. If it is still broken, unplug and replug the"
Write-Host "Ethernet cable and toggle Wi-Fi (Fn key / airplane mode) -- that always wins."
```

> **If the adapter is disabled and even this fails** (Windows 10 version 2004 or later):
> ```
> pnputil /enum-devices /class Net
> pnputil /enable-device "<the Instance ID printed above>"
> ```
> **Or with no command line at all:** right-click **Start → Device Manager → Network adapters**
> (**View → Show hidden devices** if you don't see it) → right-click the adapter with the down-arrow
> icon (*"This device is disabled. (Code 22)"*) → **Enable device**. No reboot needed.
>
> **What this script deliberately does NOT do:** it does not touch VPN, Hyper‑V or VMware adapters,
> it does not re-enable bindings other than IPv4/IPv6, and it does not run
> `netsh int ip reset` or Windows' **Network reset** — those reinstall every adapter, wipe
> all your settings, and give every adapter a new identity.

---

## 8. UI — exactly what to show

### Mode labels and one-line truths

| Control | Line |
|---|---|
| **Ethernet** | *"Ethernet carries your traffic. Wi‑Fi stays available as a backup."* |
| **Wi‑Fi** | *"Wi‑Fi carries your internet traffic. Ethernet stays connected so your NAS and printers still work — and Windows may still send DNS lookups over it."* |
| **Wi‑Fi only** | *"Ethernet has no IP address, no routes and no DNS, so it can't carry traffic — the cable stays plugged in and the adapter stays on."* |
| **Wi‑Fi only** (secondary line, always shown) | *"Windows still sends occasional link-discovery frames (LLDP/LLTD) on this cable."* |
| **Wi‑Fi only → Strict soft off** (checkbox) | *"Also stop link-discovery frames. This PC will stop appearing on other machines' network map."* |
| **Wi‑Fi only → Strict (adapter off)** (radio) | *"Switch the Ethernet adapter off completely: no traffic of any kind, and the switch port goes dark. This also stops Wake‑on‑LAN."* |
| **Automatic** | *"Hand both adapters back to Windows."* |

**The Wi‑Fi mode DNS caveat is required, not optional.** Interface metric steers *routes*, not DNS server selection. In mode 2 Ethernet keeps its DHCP DNS server (`192.168.1.1` on the dev box, persisted as `DhcpNameServer` — it survives even a cable unplug) and Windows issues parallel DNS/LLMNR/NetBT queries **across all networks** by design. The optional hardening writes `DisableSmartNameResolution = 1` (REG_DWORD) at `HKLM\SOFTWARE\Policies\Microsoft\Windows NT\DNSClient` — the *documented* value; pair it with `DisableSmartProtocolReordering` at the same key. `DisableParallelAandAAAA` is **undocumented** — if shipped, label it as such. Do not offer a "binding order" fix: network binding order was deprecated in Windows 8 and **removed** in Windows 10; the tie-break is now interface metric plus DNS client policy.

### Never write these words

*"PoE keeps working"* · *"zero disruption"* · *"the link never drops"* · *"cleaner than unplugging"* · *"Wake-on-LAN keeps working"* (say **magic-packet** Wake-on-LAN, and only after G‑WOL passes) · *"all traffic goes over Wi‑Fi"* for mode 2 without the DNS clause.

### Failure and warning strings

| Condition | Exit | Text |
|---|---|---|
| netcfg lock busy | 8 `LOCK_BUSY` | *"Windows network settings are locked by **{holder}**. Close its window and try again."* (`{holder}` comes from `AcquireWriteLock`; usually the Network Connections adapter Properties dialog.) |
| Not elevated | 2 | *"LinkSwitch's helper isn't set up. Run `linkswitch --install` once as administrator."* |
| Wi‑Fi pre-flight failed | 4 | *"Wi‑Fi isn't ready — {no radio / not connected / no address / no internet}. Nothing was changed."* |
| Captive portal on Wi‑Fi | 4 | *"Wi‑Fi is connected but hasn't reached the internet yet — sign in to the network first. Nothing was changed."* |
| G1 last link | 11 | *"Ethernet is your only working connection right now. Connect Wi‑Fi first."* |
| G2 RDP | 11 | *"You're connected to this PC over Remote Desktop through Ethernet. Turning Ethernet off would disconnect you. Nothing was changed."* |
| G3 VPN | warn | *"Your VPN is running over Ethernet. Wi‑Fi‑only will disconnect it."* |
| G4 bridge | warn banner | *"A virtual-machine bridge (VMware / Hyper‑V) is attached to this Ethernet adapter. Virtual machines can still send traffic on this cable even in Wi‑Fi‑only mode. Use Strict mode, or unbridge the VM, to stop that."* |
| `NETCFG_E_VMSWITCH_ACTIVE_OVER_ADAPTER` | 9 | *"A Hyper‑V virtual switch is using this adapter and Windows won't let LinkSwitch change it. Remove the external switch from this adapter first."* |
| `NETCFG_S_REBOOT` / `DI_NEEDREBOOT` | 10 | *"The change is staged but needs a restart to take effect."* — **not** "Done." |
| Watchdog auto-revert | 6 | *"Wi‑Fi stopped working after the switch, so LinkSwitch put Ethernet back."* |
| Crash recovery fired | toast | *"LinkSwitch put your network back after an interrupted change."* |
| Adapter not resolvable via INetCfg | 3 | *"LinkSwitch can't configure this adapter."* — never a generic failure. |
| Drift | banner | *"Something outside LinkSwitch changed this adapter."* + **[Reapply]** |
| NCSI probing disabled by policy | banner | *"Windows' internet check is turned off on this PC, so LinkSwitch can't fully verify Wi‑Fi before switching."* |

**Progress:** indeterminate spinner, never a countdown. No first-party timing data exists for a NIC bind/unbind or enable/disable; budget 1–5 s and measure on your own hardware.

---

## 9. RISK REGISTER (prioritised)

| # | Risk | Sev | Likelihood | Mitigation |
|---|---|---|---|---|
| **R1** | **User is stranded with no network and no way to fix it.** Wi‑Fi drops after the switch; or HARD OFF persists across a reboot into a Wi‑Fi that won't associate. | Critical | Medium | G1 last-link quorum (tunnels excluded) · full Wi‑Fi internet pre-flight before any mutation · 20 s watchdog with fresh-verdict requirement · `Watchdog` task repeating every 1 min for 15 min, registered *before* mutation · startup **and** logon `--recover` triggers · SOFT OFF (reversible, no devnode churn) as the default · README rescue script (§7) that needs no binary and no network. |
| **R2** | **Local privilege escalation via the journal.** `C:\ProgramData` grants Users Write+CI and `C:\ProgramData\LinkSwitch` does not exist yet, so it can be squatted before install; a RunLevel HIGHEST no-UAC task then replays attacker-chosen device IDs at boot. | Critical | Low | `icacls /setowner Administrators` **then** `/inheritance:r /grant:r` Admins:F SYSTEM:F Users:RX, before the first journal write · worker verifies owner + protected DACL on every read and refuses otherwise · every journal-sourced ID re-resolved against a live `GUID_DEVCLASS_NET` enumeration before use · the unelevated widget can only invoke a fixed set of verbs. |
| **R3** | **The netcfg write lock is leaked**, breaking `ncpa.cpl`, `netsh`, Device Manager and every other config tool machine-wide until reboot. | High | Medium | `LockGuard`/`InitGuard` RAII (drop order = `Cancel`→`Uninitialize`→`ReleaseWriteLock`) · never `unwrap()` under the lock · `catch_unwind` around the whole COM section (`panic = "abort"` is deliberately off) · `--recover` detects and reports a stuck lock. |
| **R4** | **"Zero Ethernet" is false on a machine with a VM bridge.** `vmware_bridge` and `ms_l2bridge` are Enabled on the dev box today; bridged VMs bypass the host stack entirely. | High | High | G4 detect-and-warn on `vmware_bridge`/`ms_l2bridge`/`vms_pp`/`ms_implat` · offer HARD OFF · the UI never claims "no traffic" for soft off, only "no IP" · never silently unbind the bridge. |
| **R5** | **The link bounces on unbind**, contradicting the product's core promise (and possibly the earlier claim that PoE/WoL survive). Microsoft's own docs say the operation *"restarts the network adapter."* | High | Medium | **Blocking gate G‑LINK** on real hardware into a managed switch before 1.0 · **blocking gate G‑WOL** with a real magic packet · until both pass, the UI says only *"the cable stays plugged in and the adapter stays enabled"* · PoE claim deleted outright. |
| **R6** | **Restore re-enables IPv6 the user deliberately turned off** (VPN leak protection). `ms_tcpip6` is already `False` on **nine** adapters on the dev box. | High | High | Snapshot the `(v4,v6)` tuple per adapter before the first unbind, persist it, restore exactly that · the codebase's existing "never restore a value you did not capture" rule enforced in review · rescue script warns about this explicitly. |
| **R7** | **`Apply()` returns `NETCFG_S_REBOOT` and LinkSwitch reports success** on a half-applied change (`is_ok()` is true for `S_` codes). | High | Low | Read `Apply`'s **raw** HRESULT via the vtable · distinct exit code 10 · UI says "staged", not "done". |
| **R8** | **`S_FALSE` is swallowed by `.ok()`** — believing the lock is held, or misreading `IsBoundTo`/`FindComponent`, and reporting a change that never happened. | High | Certain (if unguarded) | Raw-vtable helper for **every** S_OK/S_FALSE method: `AcquireWriteLock`, `IsWriteLocked`, `IsBoundTo`, `IsBindableTo`, `SupportsBindingInterface`, `IsEnabled`, **`FindComponent`** · one `fn com_bool(hr) -> Result<bool>` helper, no exceptions · a unit test asserting the helper maps `S_FALSE → Ok(false)`. |
| **R9** | **Silent no-op from the wrong `IsBoundTo` direction** — `adapter.IsBoundTo(protocol)` returns `S_FALSE` for every adapter, the QI succeeds, no error appears, and LinkSwitch concludes everything is already unbound. | High | Medium | Direction fixed in one helper with a doc comment · integration test comparing against `Get-NetAdapterBinding` on a scratch VM. |
| **R10** | **VPN tunnels defeat modes 1 and 2 silently.** The dev box's default route belongs to ProtonVPN with **no** interface metric; metric steering changes nothing observable while the UI claims success. | Medium | High | Detect tunnels (`IfType 131` / `TunnelType` / non-physical ifIndex on `0.0.0.0/0`) · answer "who carries traffic" with `GetBestRoute2` to a public address, not metric comparison · UI states plainly that a VPN is carrying traffic. |
| **R11** | **HARD OFF takes VMs offline or leaves a vSwitch degraded** (documented NIC Teaming analogue: Enable does not undo Disable because the virtual adapter was destroyed and recreated disabled). | Medium | Medium | G4 warning before HARD OFF · extra confirmation naming the consequences · SOFT OFF is the default. |
| **R12** | **Drift after a driver reinstall or Windows Update** recreates the class key from the INF with all protocols bound, or a vendor installer re-runs netcfg. | Medium | High | Never trust the stored mode; re-read real state at launch, on `WM_DEVICECHANGE`, and on resume · drift banner with one-click Reapply, never a silent timer re-apply · post-apply diff of **all** bindings catches the documented auto-enable side effect. |
| **R13** | **Identity goes stale.** HARD OFF changes ifIndex/LUID; a Network reset mints a new adapter GUID; a dock replug or driver reinstall changes the LUID daily. | Medium | High | Persist `{LUID, adapter_guid, device_instance_id, MAC, name, description}` and resolve in that order · treat a miss as "re-resolve and confirm with the user", never a hard failure · never key persisted state on ifIndex. |
| **R14** | **Rescue script causes collateral damage** by blanket-restoring adapters and bindings LinkSwitch never touched (22 disabled bindings on the dev box, incl. ProtonVPN's `ms_tcpip6` and Hyper‑V's deliberate metric 5000). | Medium | Medium | Script is scoped to `state.json`'s `touched` list by default, filtered to InterfaceType 6/71 and ComponentIDs `ms_tcpip`/`ms_tcpip6` · prints a plan and asks once · no `-ErrorAction SilentlyContinue` on the restore calls · `-All` is opt-in. |
| **R15** | **NLM's cached verdict commits a broken state** — a 1 s poll immediately after the mutation reads the pre-change answer. | Medium | Medium | 3 s settle · require 2 consecutive positives with a post-apply verdict timestamp · prefer `INetworkListManagerEvents` notifications over polling · per-connection `GetAdapterId()` matching, never the system-wide verdict. |
| **R16** | **`ERROR_INVALID_PARAMETER` inside the rollback path** from a naive IPv4 `SetIpInterfaceEntry` round trip (`SitePrefixLength` must be 0 for IPv4). | Medium | Low | Zero `SitePrefixLength` before every IPv4 write · rollback path covered by a test that exercises both families. |
| **R17** | **Windows 10 untested.** All verification ran on Win11 25H2 (10.0.26200). The `CLSID_CNetCfg` server moved from `netcfgx.dll` to `NetSetupShim.dll` at some point in the Win10 lifetime, with stale reports of `REGDB_E_CLASSNOTREG` on early builds. | Medium | Low | Verify `CoCreateInstance(CLSID_CNetCfg)` on a Win10 22H2 image before claiming Win10 support · README notes `pnputil /enable-device` needs Win10 2004+. |
| **R18** | **Console flash / stray elevated command** if the PowerShell escape hatch is ever used from the GUI-subsystem binary, or if `netcfg.exe` argv is ever interpolated (`netcfg -d`/`-x` wreck all networking and force a reboot). | Low | Low | `CREATE_NO_WINDOW (0x08000000)` on every spawn · shell-out gated behind a hidden config key, never default · `netcfg.exe` invoked only as `-b`/`-m` with a fully hardcoded argv. |

---

### Appendix — blocking gates before 1.0

| Gate | What to prove | Where |
|---|---|---|
| **G‑LINK** | Unbinding both protocols does not drop the electrical link. | Real NIC into a managed switch; port link counter + 100 ms `Get-NetAdapter` polling across one apply cycle. |
| **G‑WOL** | Magic-packet wake still works with the stack unbound. | Real magic packet to the soft-off adapter from another host. |
| **G‑IPV4** | Unbinding `ms_tcpip` really removes `255.255.255.255/32`, `224.0.0.0/4` and leaves `Get-NetRoute` throwing. **Currently inferred by symmetry from IPv6 only** — no adapter on the dev box has `ms_tcpip` unbound. | Scratch VM. |
| **G‑STATIC** | A statically-configured NIC survives an unbind/rebind cycle with its IP and DNS intact. Inferred from the mechanism plus strong IPv6 evidence; never directly observed. | Scratch VM with a static IPv4 NIC. |
| **G‑LOCK** | Killing the worker between `AcquireWriteLock` and `ReleaseWriteLock` releases the lock on process death. | Kill mid-flight, then check `IsWriteLocked`. |
| **G‑WIN10** | `CoCreateInstance(CLSID_CNetCfg)` succeeds on Win10 22H2. | Win10 image. |
| **G‑BLIP** | Wall-clock duration of `UnbindFrom`+`Apply`, the connectivity blip, and DHCP re-acquisition after `BindTo`+`Apply`. No first-party numbers exist. | Instrument on 2–3 chipsets. |

**Test-script rule:** in every acceptance script, the CIM **throw** is the PASS condition. `Get-NetRoute`, `Get-NetIPInterface`, `Get-NetIPAddress` and `Get-NetNeighbor` all throw `No matching MSFT_Net* objects found` for an unbound family — they do not return an empty set. **Never use `-ErrorAction SilentlyContinue`** there; it makes a real failure (wrong ifIndex, broken WMI) indistinguishable from a pass. Use `Assert-Gone { … -ErrorAction Stop }` catching `Microsoft.Management.Infrastructure.CimException`, or query `Get-CimInstance -Namespace ROOT/StandardCimv2` directly, which genuinely returns empty. And always pass `-IncludeHidden` — the dev box has 9 adapters with `ms_tcpip6=False` and 4 with it True, and the four True ones (6to4, Teredo, IP‑HTTPS, vEthernet) are exactly the tunnel pseudo-interfaces a non-hidden enumeration misses.