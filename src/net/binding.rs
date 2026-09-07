//! Binding and unbinding TCP/IP on an adapter — the "Wi-Fi only" mechanism.
//!
//! Unbinding `ms_tcpip` and `ms_tcpip6` removes the adapter's entire IP presence while leaving
//! the NIC enabled and the cable plugged in: no addresses, no routes of any kind, no DNS
//! servers, no DHCP, and no entry in the network list. NetBIOS and SMB die with it, because
//! they bind to `\Device\Tcpip_{GUID}` rather than to the adapter directly — which is why this
//! module touches only the two TCP/IP components and nothing else.
//!
//! # What this does *not* achieve
//!
//! It removes zero **IP**. It does not achieve zero **networking**, and the UI must not claim
//! otherwise. Reading the NIC's own `Linkage\UpperBind` on the development machine shows what
//! still has a path to the copper once `Tcpip` is gone:
//!
//! ```text
//! lltdio  MsLldp  Ndisuio  RasPppoe  RDMANDK  rspndr  Tcpip  VMnetBridge
//! ```
//!
//! `MsLldp` keeps advertising the machine to switches; `rspndr` actively answers other Windows
//! machines' network-map probes, and LLTD is explicitly IP-independent. Most consequentially,
//! `VMnetBridge` is bound on this machine right now, and a bridged VM puts *its own* MAC and IP
//! on that wire at layer 2, entirely underneath the host stack — so on a machine with VM
//! bridging, "zero Ethernet" would simply be false. [`bridge_warning`] detects that case.
//!
//! Only disabling the device achieves literal zero frames, at the cost of the link going down.

use std::time::Duration;

use windows::core::{w, Interface, Result, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{E_UNEXPECTED, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::NetworkManagement::NetManagement::{
    IEnumNetCfgComponent, INetCfg, INetCfgComponent, INetCfgComponentBindings, INetCfgLock,
    NETCFG_E_ADAPTER_NOT_FOUND, NETCFG_E_NO_WRITE_LOCK, NETCFG_S_REBOOT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};

/// `CLSID_CNetCfg` from netcfgx.h. windows-rs does not generate it.
const CLSID_CNETCFG: GUID = GUID::from_u128(0x5b035261_40f9_11d1_aaec_00805fc1270e);

/// The network device class.
const GUID_DEVCLASS_NET: GUID = GUID::from_u128(0x4d36e972_e325_11ce_bfc1_08002be10318);

/// Component ids. Matching is case-insensitive. There is no `ms_tcpip6` constant in the crate,
/// so both are spelled out for symmetry.
const CID_MS_TCPIP: PCWSTR = w!("MS_TCPIP");
const CID_MS_TCPIP6: PCWSTR = w!("MS_TCPIP6");

/// Which IP protocols are bound to an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BindingState {
    pub v4: bool,
    pub v6: bool,
}

impl BindingState {
    pub fn any(self) -> bool {
        self.v4 || self.v6
    }
    pub fn as_tuple(self) -> (bool, bool) {
        (self.v4, self.v6)
    }
}

#[derive(Debug)]
pub enum BindError {
    /// Another program holds the machine-wide network-configuration lock.
    Locked { holder: Option<String> },
    /// The adapter is not something INetCfg can configure.
    AdapterNotFound,
    /// A Hyper-V external virtual switch sits on this NIC.
    VSwitchActive,
    NotElevated,
    Com(String),
}

impl BindError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Locked { holder } => format!(
                "Windows' network configuration is locked by {}. Close it and try again.",
                holder.as_deref().unwrap_or("another program")
            ),
            Self::AdapterNotFound => {
                "This adapter cannot be configured by LinkSwitch.".into()
            }
            Self::VSwitchActive => {
                "A Hyper-V virtual switch is using this adapter, so its IP stack cannot be \
                 detached. Remove the external switch first."
                    .into()
            }
            Self::NotElevated => {
                "Changing an adapter's protocol bindings needs administrator rights.".into()
            }
            // Windows error text sometimes already ends with a period and sometimes does not,
            // so normalise rather than appending blindly.
            Self::Com(e) => {
                let t = e.trim().trim_end_matches('.');
                format!("Windows refused the change: {t}.")
            }
        }
    }
}

fn map_err(e: windows::core::Error) -> BindError {
    let code = e.code();
    if code == NETCFG_E_ADAPTER_NOT_FOUND {
        BindError::AdapterNotFound
    } else if code == NETCFG_E_NO_WRITE_LOCK {
        BindError::Locked { holder: None }
    } else if code == windows::Win32::NetworkManagement::NetManagement::NETCFG_E_VMSWITCH_ACTIVE_OVER_ADAPTER
    {
        BindError::VSwitchActive
    } else if code == windows::Win32::Foundation::E_ACCESSDENIED {
        BindError::NotElevated
    } else {
        BindError::Com(e.to_string())
    }
}

// --- raw vtable helpers ------------------------------------------------------------------
//
// Several of these COM methods answer a *question* by returning S_OK or S_FALSE. windows-rs
// wraps them with `.ok()`, whose success test is literally `hr >= 0` -- so S_FALSE becomes
// `Ok(())` and the answer is destroyed. For AcquireWriteLock that means believing you hold a
// lock you do not, and then failing later with NETCFG_E_NO_WRITE_LOCK. Every question-shaped
// call therefore goes through the raw vtable.

/// SAFETY: `p` must be null or a `CoTaskMemAlloc`'d NUL-terminated wide string that we own.
unsafe fn take_pwstr(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.as_ptr() as *const core::ffi::c_void));
    s
}

enum LockOutcome {
    Acquired,
    Busy { holder: Option<String> },
}

/// SAFETY: `lock` must be a live `INetCfgLock`.
unsafe fn acquire_write_lock(lock: &INetCfgLock, ms: u32) -> Result<LockOutcome> {
    let mut holder = PWSTR::null();
    let hr: HRESULT = (Interface::vtable(lock).AcquireWriteLock)(
        Interface::as_raw(lock),
        ms,
        w!("LinkSwitch"),
        &mut holder as *mut PWSTR,
    );
    let who = take_pwstr(holder);
    match hr {
        S_OK => Ok(LockOutcome::Acquired),
        // Documented as "the wait time elapsed before the OS granted the lock" -- a failure to
        // acquire, despite being a success HRESULT.
        S_FALSE => Ok(LockOutcome::Busy { holder: who }),
        e => Err(e.into()),
    }
}

/// Is `protocol` bound to `adapter`?
///
/// The bindings interface must come from the **protocol**, with the adapter as the argument.
/// The intuitive reverse compiles, runs, and answers S_FALSE for every adapter on the machine
/// including provably-bound ones -- the QueryInterface on the adapter succeeds, so there is no
/// error to notice.
///
/// SAFETY: both interfaces must be live.
unsafe fn proto_is_bound_to(b: &INetCfgComponentBindings, adapter: &INetCfgComponent) -> Result<bool> {
    let hr = (Interface::vtable(b).IsBoundTo)(Interface::as_raw(b), Interface::as_raw(adapter));
    if hr.is_err() {
        return Err(hr.into());
    }
    Ok(hr == S_OK)
}

/// Find an installed component by id. `S_FALSE` means "not installed", which is data rather
/// than an error.
///
/// SAFETY: `cfg` must be a live, initialised `INetCfg`.
unsafe fn find_component(cfg: &INetCfg, id: PCWSTR) -> Result<Option<INetCfgComponent>> {
    let mut out: Option<INetCfgComponent> = None;
    let hr = (Interface::vtable(cfg).FindComponent)(
        Interface::as_raw(cfg),
        id,
        &mut out as *mut _ as *mut *mut core::ffi::c_void,
    );
    if hr.is_err() {
        return Err(hr.into());
    }
    Ok(if hr == S_OK { out } else { None })
}

/// SAFETY: `cfg` must be a live, initialised `INetCfg`.
unsafe fn find_adapter(cfg: &INetCfg, want: GUID) -> Result<INetCfgComponent> {
    let mut en: Option<IEnumNetCfgComponent> = None;
    cfg.EnumComponents(&GUID_DEVCLASS_NET, Some(&mut en))?;
    let Some(en) = en else {
        return Err(E_UNEXPECTED.into());
    };
    loop {
        let mut raw: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut fetched = 0u32;
        let hr = (Interface::vtable(&en).Next)(Interface::as_raw(&en), 1, &mut raw, &mut fetched);
        if hr != S_OK || fetched == 0 || raw.is_null() {
            break;
        }
        let c = INetCfgComponent::from_raw(raw);
        let mut g = GUID::zeroed();
        if c.GetInstanceGuid(Some(&mut g)).is_ok() && g == want {
            return Ok(c);
        }
    }
    Err(NETCFG_E_ADAPTER_NOT_FOUND.into())
}

// --- RAII guards -------------------------------------------------------------------------
//
// The naive `?`-everywhere version leaks the machine-wide network-configuration lock, which
// would leave every other network-settings tool on the machine unable to work until reboot.
// `panic = "abort"` is deliberately not set in Cargo.toml, so unwind safety matters here.

struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    fn enter() -> Result<Self> {
        // CLSID_CNetCfg is ThreadingModel=Both; netcfg notify objects can raise UI, so STA.
        // RPC_E_CHANGED_MODE means the thread is already MTA: proceed, but do not uninitialise
        // something we did not initialise.
        // SAFETY: balanced by Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self { uninit: false });
        }
        hr.ok()?;
        Ok(Self { uninit: true })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: balances the CoInitializeEx in `enter`.
            unsafe { CoUninitialize() };
        }
    }
}

struct LockGuard<'a>(&'a INetCfgLock);

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: the lock was acquired by this thread.
        unsafe {
            let _ = self.0.ReleaseWriteLock();
        }
    }
}

struct InitGuard<'a> {
    cfg: &'a INetCfg,
    committed: bool,
}

impl Drop for InitGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: `cfg` is live and was initialised.
        unsafe {
            if !self.committed {
                let _ = self.cfg.Cancel();
            }
            let _ = self.cfg.Uninitialize();
        }
    }
}

/// Parse `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` as it appears in
/// `IP_ADAPTER_ADDRESSES::AdapterName`.
pub fn parse_guid(s: &str) -> Option<GUID> {
    let t = s.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = t.split('-').collect();
    if parts.len() != 5 || parts[0].len() != 8 || parts[1].len() != 4 || parts[2].len() != 4 {
        return None;
    }
    if parts[3].len() != 4 || parts[4].len() != 12 {
        return None;
    }
    let d1 = u32::from_str_radix(parts[0], 16).ok()?;
    let d2 = u16::from_str_radix(parts[1], 16).ok()?;
    let d3 = u16::from_str_radix(parts[2], 16).ok()?;
    let mut d4 = [0u8; 8];
    let tail = format!("{}{}", parts[3], parts[4]);
    for (i, b) in d4.iter_mut().enumerate() {
        *b = u8::from_str_radix(&tail[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(GUID::from_values(d1, d2, d3, d4))
}

/// The outcome of a binding change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindOutcome {
    pub changed: bool,
    /// Windows wants a reboot before the change is fully in effect.
    pub needs_reboot: bool,
}

/// Read which IP protocols are currently bound to an adapter. Needs no elevation.
pub fn read(adapter_guid: &str) -> std::result::Result<BindingState, BindError> {
    let guid = parse_guid(adapter_guid).ok_or(BindError::AdapterNotFound)?;
    let _com = ComGuard::enter().map_err(map_err)?;

    // SAFETY: standard in-process COM activation; every interface is released by Drop.
    unsafe {
        let netcfg: INetCfg =
            CoCreateInstance(&CLSID_CNETCFG, None, CLSCTX_INPROC_SERVER).map_err(map_err)?;
        netcfg.Initialize(None).map_err(map_err)?;
        let init = InitGuard {
            cfg: &netcfg,
            committed: true, // read-only: nothing to cancel
        };

        let adapter = find_adapter(&netcfg, guid).map_err(map_err)?;
        let mut state = BindingState::default();
        for (id, slot) in [
            (CID_MS_TCPIP, &mut state.v4 as *mut bool),
            (CID_MS_TCPIP6, &mut state.v6 as *mut bool),
        ] {
            if let Some(p) = find_component(&netcfg, id).map_err(map_err)? {
                let b: INetCfgComponentBindings = p.cast().map_err(map_err)?;
                *slot = proto_is_bound_to(&b, &adapter).map_err(map_err)?;
            }
        }
        drop(init);
        Ok(state)
    }
}

/// Bind or unbind TCP/IP on an adapter. Requires elevation.
///
/// `want` is the *exact* target state. On restore, pass the tuple captured before the change --
/// never a hardcoded `(true, true)`. On this machine a VPN has already unbound `ms_tcpip6` for
/// IPv6 leak protection, and blindly rebinding it would switch IPv6 back on and leak the user's
/// real address.
pub fn set(adapter_guid: &str, want: BindingState) -> std::result::Result<BindOutcome, BindError> {
    let guid = parse_guid(adapter_guid).ok_or(BindError::AdapterNotFound)?;
    let _com = ComGuard::enter().map_err(map_err)?;

    // SAFETY: standard in-process COM activation. The guards below release the write lock and
    // uninitialise on every path, including an unwind.
    unsafe {
        let netcfg: INetCfg =
            CoCreateInstance(&CLSID_CNETCFG, None, CLSCTX_INPROC_SERVER).map_err(map_err)?;
        let lock: INetCfgLock = netcfg.cast().map_err(map_err)?;

        // The lock must be taken BEFORE Initialize; taking it after returns
        // NETCFG_E_ALREADY_INITIALIZED.
        let mut outcome = acquire_write_lock(&lock, 5_000).map_err(map_err)?;
        if let LockOutcome::Busy { .. } = outcome {
            std::thread::sleep(Duration::from_secs(2));
            outcome = acquire_write_lock(&lock, 5_000).map_err(map_err)?;
        }
        if let LockOutcome::Busy { holder } = outcome {
            return Err(BindError::Locked { holder });
        }
        // Declared before InitGuard so it drops *after* it: Cancel/Uninitialize, then release.
        let _lock_guard = LockGuard(&lock);

        netcfg.Initialize(None).map_err(map_err)?;
        let mut init = InitGuard {
            cfg: &netcfg,
            committed: false,
        };

        let v4 = find_component(&netcfg, CID_MS_TCPIP).map_err(map_err)?;
        let v6 = find_component(&netcfg, CID_MS_TCPIP6).map_err(map_err)?;
        if v4.is_none() && want.v4 {
            // Refuse rather than report a success that did not happen.
            return Err(BindError::Com(
                "the IPv4 protocol is not installed on this machine".into(),
            ));
        }

        let adapter = find_adapter(&netcfg, guid).map_err(map_err)?;

        let mut changed = false;
        for (proto, want_bound) in [(v4.as_ref(), want.v4), (v6.as_ref(), want.v6)] {
            let Some(p) = proto else { continue };
            let b: INetCfgComponentBindings = p.cast().map_err(map_err)?;
            if proto_is_bound_to(&b, &adapter).map_err(map_err)? == want_bound {
                continue;
            }
            if want_bound {
                b.BindTo(&adapter).map_err(map_err)?;
            } else {
                b.UnbindFrom(&adapter).map_err(map_err)?;
            }
            changed = true;
        }

        let mut needs_reboot = false;
        if changed {
            // Apply's HRESULT must be read raw: NETCFG_S_REBOOT is a *success* code, so `?`
            // swallows it and a half-applied change gets reported as done.
            let hr = (Interface::vtable(&netcfg).Apply)(Interface::as_raw(&netcfg));
            if hr.is_err() {
                return Err(map_err(hr.into()));
            }
            needs_reboot = hr == NETCFG_S_REBOOT;
        }
        init.committed = true;
        drop(init);

        Ok(BindOutcome {
            changed,
            needs_reboot,
        })
    }
}

/// Bridging components that would still put frames on this wire with TCP/IP unbound.
///
/// This is the difference between "zero Ethernet" being imprecise and being false. A bridged
/// VM puts its own MAC and its own IP on the wire at layer 2, entirely beneath the host's
/// stack, so unbinding host TCP/IP does nothing to it. `vmware_bridge` and `ms_l2bridge` are
/// both bound on the development machine right now.
pub const BRIDGE_COMPONENTS: &[(&str, &str)] = &[
    ("VMWARE_BRIDGE", "VMware bridged networking"),
    ("MS_L2BRIDGE", "the Windows bridge driver"),
    ("VMS_PP", "a Hyper-V virtual switch"),
];

/// Which bridging components are bound to this adapter. Read-only, no elevation.
pub fn bridges(adapter_guid: &str) -> Vec<&'static str> {
    let Some(guid) = parse_guid(adapter_guid) else {
        return Vec::new();
    };
    let Ok(_com) = ComGuard::enter() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    // SAFETY: standard in-process COM activation; read-only, and every interface is released
    // by Drop.
    unsafe {
        let Ok(netcfg) = CoCreateInstance::<_, INetCfg>(&CLSID_CNETCFG, None, CLSCTX_INPROC_SERVER)
        else {
            return Vec::new();
        };
        if netcfg.Initialize(None).is_err() {
            return Vec::new();
        }
        let init = InitGuard {
            cfg: &netcfg,
            committed: true, // read-only
        };
        if let Ok(adapter) = find_adapter(&netcfg, guid) {
            for (id, label) in BRIDGE_COMPONENTS {
                let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
                let Ok(Some(c)) = find_component(&netcfg, PCWSTR(wide.as_ptr())) else {
                    continue;
                };
                let Ok(b) = c.cast::<INetCfgComponentBindings>() else {
                    continue;
                };
                if proto_is_bound_to(&b, &adapter).unwrap_or(false) {
                    found.push(*label);
                }
            }
        }
        drop(init);
    }
    found
}

/// A sentence for the UI when silencing this adapter would not actually silence the wire.
pub fn bridge_warning(adapter_guid: &str) -> Option<String> {
    let b = bridges(adapter_guid);
    if b.is_empty() {
        return None;
    }
    // Verb agreement matters here because the list is very often plural: on the development
    // machine both VMware bridging and the Windows bridge driver are bound at once.
    let verb = if b.len() == 1 { "uses" } else { "use" };
    Some(format!(
        "{} still {verb} this adapter directly, so the wire will not be silent.",
        join_list(&b)
    ))
}

fn join_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [a] => (*a).to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_parses_with_and_without_braces() {
        let a = parse_guid("{66666666-7777-8888-9999-AAAAAAAAAAAA}").unwrap();
        let b = parse_guid("66666666-7777-8888-9999-aaaaaaaaaaaa").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, GUID::from_u128(0x66666666_7777_8888_9999_aaaaaaaaaaaa));
    }

    #[test]
    fn malformed_guids_are_rejected_rather_than_silently_wrong() {
        // A bad GUID must not resolve to some other adapter.
        for bad in [
            "",
            "{}",
            "not-a-guid",
            "{66666666-7777-8888-9999}",
            "{66666666-7777-8888-9999-AAAAAAAAAAAA-extra}",
            "{ZZZZDBD0-7777-8888-9999-AAAAAAAAAAAA}",
            "{66666666-7777-8888-9999-AAAAAAAAAAA}",
        ] {
            assert!(parse_guid(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn binding_state_helpers() {
        assert!(!BindingState { v4: false, v6: false }.any());
        assert!(BindingState { v4: true, v6: false }.any());
        assert_eq!(BindingState { v4: true, v6: false }.as_tuple(), (true, false));
    }

    #[test]
    fn bridge_warning_agrees_in_number() {
        // Both branches are reachable on ordinary machines, so both need to read correctly.
        assert_eq!(
            format!("{} still {} x.", join_list(&["A"]), "uses"),
            "A still uses x."
        );
        assert_eq!(
            format!("{} still {} x.", join_list(&["A", "B"]), "use"),
            "A and B still use x."
        );
    }

    #[test]
    fn list_joining_reads_naturally() {
        assert_eq!(join_list(&[]), "");
        assert_eq!(join_list(&["A"]), "A");
        assert_eq!(join_list(&["A", "B"]), "A and B");
        assert_eq!(join_list(&["A", "B", "C"]), "A, B, and C");
    }

    #[test]
    fn every_bind_error_has_an_actionable_message() {
        let errs = [
            BindError::Locked {
                holder: Some("Network Connections".into()),
            },
            BindError::Locked { holder: None },
            BindError::AdapterNotFound,
            BindError::VSwitchActive,
            BindError::NotElevated,
            BindError::Com("0x80004005".into()),
        ];
        for e in &errs {
            let m = e.user_message();
            assert!(!m.is_empty());
            assert!(m.ends_with('.'), "message should be a sentence: {m}");
        }
        // The holder's name is surfaced so the user knows what to close.
        assert!(errs[0].user_message().contains("Network Connections"));
        assert!(errs[1].user_message().contains("another program"));

        // Windows error text arrives with and without a trailing period; neither should
        // produce a double period or a bare fragment.
        assert!(BindError::Com("it broke".into())
            .user_message()
            .ends_with("it broke."));
        assert!(BindError::Com("it broke.".into())
            .user_message()
            .ends_with("it broke."));
    }
}
