//! Wi-Fi status, and connecting on demand.
//!
//! # Why the worker has to connect Wi-Fi itself
//!
//! Windows Connection Manager's *minimize the number of simultaneous connections* policy is
//! **enabled by default**, and the registry value being absent means enabled, not disabled --
//! measured on the development machine as `WcmQueryProperty -> fValue = true, fIsGroupPolicy =
//! false` with no value present.
//!
//! With that policy on, Windows blocks a new *automatic* connection whenever a preferred network
//! type is already up, and Ethernet is always the preferred type. So after any boot or resume
//! with the cable plugged in, Wi-Fi will not auto-associate at all, and a metric flip would move
//! traffic onto a link that is not there. Windows does keep networks the user connected manually
//! during the session, which is the door this module walks through: the worker associates Wi-Fi
//! deliberately, then and only then applies the metric change.

use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    dot11_BSS_type_any, dot11_radio_state_off, wlan_connection_mode_profile,
    wlan_interface_state_connected, wlan_interface_state_not_ready,
    wlan_intf_opcode_current_connection,
    wlan_intf_opcode_radio_state, WlanCloseHandle, WlanConnect, WlanEnumInterfaces,
    WlanFreeMemory, WlanGetAvailableNetworkList, WlanGetProfileList, WlanOpenHandle,
    WlanQueryInterface, WLAN_AVAILABLE_NETWORK_LIST, WLAN_CONNECTION_ATTRIBUTES,
    WLAN_CONNECTION_PARAMETERS, WLAN_INTERFACE_INFO_LIST, WLAN_INTERFACE_STATE,
    WLAN_PROFILE_INFO_LIST, WLAN_RADIO_STATE,
};

/// Why Wi-Fi could not be brought up. Each variant is a different thing to tell the user; a bare
/// timeout would collapse "airplane mode", "no saved network" and "wrong password" into one
/// useless message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiError {
    /// wlanapi could not be opened, or the WLAN AutoConfig service is not running.
    ServiceUnavailable(u32),
    /// No Wi-Fi adapter at all -- a desktop.
    NoAdapter,
    /// The adapter exists but is not ready: driver starting, or the device is disabled.
    NotReady,
    /// Airplane mode, or the physical Wi-Fi switch is off.
    RadioOff { hardware: bool },
    /// No saved Wi-Fi profile to connect to.
    NoProfile,
    /// Networks are saved, but none of them is currently within range.
    NoneInRange { saved: usize, visible: usize },
    /// `WlanConnect` was rejected outright.
    ConnectFailed { code: u32, profile: String },
    /// Association was requested but never completed.
    Timeout { profile: String, waited: Duration },
}

impl WifiError {
    /// A sentence to show the user, phrased as what to do about it.
    pub fn user_message(&self) -> String {
        match self {
            Self::ServiceUnavailable(c) => {
                format!("Windows' WLAN service is not responding (error {c}).")
            }
            Self::NoAdapter => "No Wi-Fi adapter was found on this machine.".into(),
            Self::NotReady => {
                "The Wi-Fi adapter is not ready. It may be disabled in Device Manager.".into()
            }
            Self::RadioOff { hardware: true } => {
                "The Wi-Fi radio is switched off by a hardware switch on this machine.".into()
            }
            Self::RadioOff { hardware: false } => {
                "Wi-Fi is turned off. Turn it on, or leave airplane mode, and try again.".into()
            }
            Self::NoProfile => {
                "No saved Wi-Fi network. Connect to your network once in Windows, then LinkSwitch \
                 can reconnect it for you."
                    .into()
            }
            Self::NoneInRange { saved, visible } => format!(
                "None of your {saved} saved Wi-Fi networks is in range ({visible} network(s) \
                 visible). Move into range of one, or connect manually once."
            ),
            Self::ConnectFailed { code, profile } => {
                format!("Windows refused to connect to \"{profile}\" (error {code}).")
            }
            Self::Timeout { profile, waited } => format!(
                "Wi-Fi did not finish connecting to \"{profile}\" within {}s. It may be out of \
                 range, or the saved password may be wrong.",
                waited.as_secs()
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WifiStatus {
    pub ssid: Option<String>,
    pub profile: Option<String>,
    /// 0-100, as Windows reports it.
    pub signal_quality: Option<u32>,
    pub connected: bool,
    pub radio_on: bool,
}

/// RAII wrapper: wlanapi handles leak silently otherwise, and this process is elevated.
struct WlanHandle(HANDLE);

impl WlanHandle {
    fn open() -> Result<Self, WifiError> {
        let mut negotiated = 0u32;
        let mut h = HANDLE::default();
        // Client version 2 = Windows Vista and later.
        // SAFETY: out-params are valid locals for the duration of the call.
        let rc = unsafe { WlanOpenHandle(2, None, &mut negotiated, &mut h) };
        if rc != 0 {
            return Err(WifiError::ServiceUnavailable(rc));
        }
        Ok(Self(h))
    }
}

impl Drop for WlanHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful WlanOpenHandle and is closed exactly once.
        unsafe {
            let _ = WlanCloseHandle(self.0, None);
        }
    }
}

/// A `WlanFreeMemory`-owning pointer. Every wlanapi query allocates, and every early return in
/// this module would otherwise leak.
struct WlanMem<T>(*mut T);

impl<T> WlanMem<T> {
    /// SAFETY: `p` must be a non-null pointer returned by a wlanapi query.
    unsafe fn new(p: *mut T) -> Self {
        Self(p)
    }
    /// SAFETY: the pointer must still be valid and correctly typed.
    unsafe fn get(&self) -> &T {
        &*self.0
    }
}

impl<T> Drop for WlanMem<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from wlanapi and is freed exactly once.
            unsafe { WlanFreeMemory(self.0 as *const core::ffi::c_void) };
        }
    }
}

/// The first Wi-Fi interface, with its state.
fn first_interface(h: &WlanHandle) -> Result<(GUID, WLAN_INTERFACE_STATE), WifiError> {
    let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
    // SAFETY: `list` is a valid out-pointer; ownership passes to WlanMem below.
    let rc = unsafe { WlanEnumInterfaces(h.0, None, &mut list) };
    if rc != 0 || list.is_null() {
        return Err(WifiError::ServiceUnavailable(rc));
    }
    // SAFETY: `list` is non-null and came from WlanEnumInterfaces.
    let list = unsafe { WlanMem::new(list) };
    // SAFETY: the allocation is live for the lifetime of `list`.
    let l = unsafe { list.get() };
    if l.dwNumberOfItems == 0 {
        return Err(WifiError::NoAdapter);
    }
    // InterfaceInfo is a C flexible array member declared as a 1-element Rust array, so it must
    // be read through a raw pointer rather than by indexing.
    // SAFETY: the allocation holds `dwNumberOfItems` entries; we read only the first.
    let info = unsafe { &*l.InterfaceInfo.as_ptr() };
    Ok((info.InterfaceGuid, info.isState))
}

/// SAFETY: `guid` must name a live WLAN interface and `T` must match the opcode's payload type.
unsafe fn query<T>(
    h: &WlanHandle,
    guid: &GUID,
    opcode: windows::Win32::NetworkManagement::WiFi::WLAN_INTF_OPCODE,
) -> Option<WlanMem<T>> {
    let mut size = 0u32;
    let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
    let rc = WlanQueryInterface(h.0, guid, opcode, None, &mut size, &mut data, None);
    if rc != 0 || data.is_null() || (size as usize) < std::mem::size_of::<T>() {
        if !data.is_null() {
            WlanFreeMemory(data);
        }
        return None;
    }
    Some(WlanMem::new(data as *mut T))
}

fn radio_state(h: &WlanHandle, guid: &GUID) -> Option<(bool, bool)> {
    // SAFETY: the radio-state opcode returns a WLAN_RADIO_STATE.
    let mem = unsafe { query::<WLAN_RADIO_STATE>(h, guid, wlan_intf_opcode_radio_state) }?;
    // SAFETY: allocation live for the lifetime of `mem`.
    let rs = unsafe { mem.get() };
    let n = (rs.dwNumberOfPhys as usize).min(rs.PhyRadioState.len());
    // "On" if any PHY is on: a 6E adapter reports several, and a disabled 2.4 GHz PHY does not
    // mean the radio is off.
    let mut soft_off = n > 0;
    let mut hard_off = n > 0;
    for p in &rs.PhyRadioState[..n] {
        if p.dot11SoftwareRadioState != dot11_radio_state_off {
            soft_off = false;
        }
        if p.dot11HardwareRadioState != dot11_radio_state_off {
            hard_off = false;
        }
    }
    Some((!soft_off, !hard_off))
}

fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Current Wi-Fi state. Unprivileged; called by the widget on every refresh.
pub fn status() -> Option<WifiStatus> {
    let h = WlanHandle::open().ok()?;
    let (guid, state) = first_interface(&h).ok()?;

    let (soft_on, hard_on) = radio_state(&h, &guid).unwrap_or((true, true));
    let connected = state == wlan_interface_state_connected;

    let mut ssid = None;
    let mut profile = None;
    let mut signal_quality = None;

    if connected {
        // SAFETY: the current-connection opcode returns WLAN_CONNECTION_ATTRIBUTES.
        if let Some(mem) =
            unsafe { query::<WLAN_CONNECTION_ATTRIBUTES>(&h, &guid, wlan_intf_opcode_current_connection) }
        {
            // SAFETY: allocation live for the lifetime of `mem`.
            let c = unsafe { mem.get() };
            let a = &c.wlanAssociationAttributes;
            let len = (a.dot11Ssid.uSSIDLength as usize).min(a.dot11Ssid.ucSSID.len());
            if len > 0 {
                ssid = Some(String::from_utf8_lossy(&a.dot11Ssid.ucSSID[..len]).into_owned());
            }
            signal_quality = Some(a.wlanSignalQuality);
            let p = utf16_to_string(&c.strProfileName);
            if !p.is_empty() {
                profile = Some(p);
            }
        }
    }

    Some(WifiStatus {
        ssid,
        profile,
        signal_quality,
        connected,
        radio_on: soft_on && hard_on,
    })
}

/// Saved Wi-Fi profiles, in Windows' own preference order.
fn profiles(h: &WlanHandle, guid: &GUID) -> Vec<String> {
    let mut list: *mut WLAN_PROFILE_INFO_LIST = std::ptr::null_mut();
    // SAFETY: `list` is a valid out-pointer; ownership passes to WlanMem.
    let rc = unsafe { WlanGetProfileList(h.0, guid, None, &mut list) };
    if rc != 0 || list.is_null() {
        return Vec::new();
    }
    // SAFETY: non-null, from WlanGetProfileList.
    let list = unsafe { WlanMem::new(list) };
    // SAFETY: allocation live for the lifetime of `list`.
    let l = unsafe { list.get() };
    let n = l.dwNumberOfItems as usize;
    let mut out = Vec::with_capacity(n);
    // SAFETY: flexible array member holding `dwNumberOfItems` entries.
    let items = unsafe { std::slice::from_raw_parts(l.ProfileInfo.as_ptr(), n) };
    for it in items {
        let name = utf16_to_string(&it.strProfileName);
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// Profile names of networks that are both visible right now and connectable.
///
/// This is the half `WlanGetProfileList` cannot answer. Profile order is *preference* order,
/// which says nothing about what is actually on the air -- on the development machine there are
/// 31 saved profiles and the first is a phone hotspot that is almost never present. Connecting
/// to `profiles[0]` therefore burned the whole association timeout and reported a useless
/// "timed out" every time.
fn available_profiles(h: &WlanHandle, guid: &GUID) -> Vec<(String, u32)> {
    let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
    // SAFETY: `list` is a valid out-pointer; ownership passes to WlanMem.
    let rc = unsafe { WlanGetAvailableNetworkList(h.0, guid, 0, None, &mut list) };
    if rc != 0 || list.is_null() {
        return Vec::new();
    }
    // SAFETY: non-null, from WlanGetAvailableNetworkList.
    let list = unsafe { WlanMem::new(list) };
    // SAFETY: allocation live for the lifetime of `list`.
    let l = unsafe { list.get() };
    let n = l.dwNumberOfItems as usize;
    // SAFETY: flexible array member holding `dwNumberOfItems` entries.
    let items = unsafe { std::slice::from_raw_parts(l.Network.as_ptr(), n) };
    let mut out: Vec<(String, u32)> = Vec::new();
    for it in items {
        if !it.bNetworkConnectable.as_bool() {
            continue;
        }
        let name = utf16_to_string(&it.strProfileName);
        // An empty profile name means a visible network we have no saved credentials for.
        if name.is_empty() {
            continue;
        }
        // The same SSID appears once per BSS; keep the strongest sighting.
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(e) => e.1 = e.1.max(it.wlanSignalQuality),
            None => out.push((name, it.wlanSignalQuality)),
        }
    }
    out
}

/// Choose which network to join.
///
/// The network we were last on wins if it is on the air. Otherwise the **strongest** saved
/// network in range -- not the most preferred. Windows' preference order is whatever sequence
/// the user happened to join things in over the years, so on a machine with 31 saved profiles
/// it is a poor guess at "the network I am sitting next to"; signal strength is a much better
/// one. Pure, so the rule is testable without a radio.
fn choose_profile(
    hint: Option<&str>,
    saved: &[String],
    visible: &[(String, u32)],
) -> Option<String> {
    if let Some(h) = hint {
        if saved.iter().any(|s| s == h) && visible.iter().any(|(v, _)| v == h) {
            return Some(h.to_string());
        }
    }
    visible
        .iter()
        .filter(|(v, _)| saved.contains(v))
        .max_by_key(|(_, q)| *q)
        .map(|(v, _)| v.clone())
}

/// Ensure Wi-Fi is associated, connecting it if necessary.
///
/// `preferred` is the profile last seen connected, tried first so a laptop with a dozen saved
/// networks reconnects to the right one rather than to whatever Windows lists first.
///
/// Returns the profile that ended up connected.
pub fn ensure_connected(
    preferred: Option<&str>,
    timeout: Duration,
) -> Result<String, WifiError> {
    let h = WlanHandle::open()?;
    let (guid, state) = first_interface(&h)?;

    if state == wlan_interface_state_not_ready {
        return Err(WifiError::NotReady);
    }

    if state == wlan_interface_state_connected {
        return Ok(status().and_then(|s| s.profile).unwrap_or_default());
    }

    // Check the radio before blaming anything else: a hardware switch or airplane mode is the
    // most common reason Wi-Fi "won't connect", and it is not something we can fix.
    if let Some((soft_on, hard_on)) = radio_state(&h, &guid) {
        if !hard_on {
            return Err(WifiError::RadioOff { hardware: true });
        }
        if !soft_on {
            return Err(WifiError::RadioOff { hardware: false });
        }
    }

    let saved = profiles(&h, &guid);
    if saved.is_empty() {
        return Err(WifiError::NoProfile);
    }
    let visible = available_profiles(&h, &guid);
    let Some(profile) = choose_profile(preferred, &saved, &visible) else {
        return Err(WifiError::NoneInRange {
            saved: saved.len(),
            visible: visible.len(),
        });
    };

    // A profile connection, not `wlan_connection_mode_auto`: auto mode is documented as valid
    // only for WlanConnect's "auto" discovery variants and is rejected with
    // ERROR_INVALID_PARAMETER here, which would make every switch-to-Wi-Fi fail identically.
    let wide: Vec<u16> = profile.encode_utf16().chain(std::iter::once(0)).collect();
    let params = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: PCWSTR(wide.as_ptr()),
        pDot11Ssid: std::ptr::null_mut(),
        pDesiredBssidList: std::ptr::null_mut(),
        dot11BssType: dot11_BSS_type_any,
        dwFlags: 0,
    };

    // SAFETY: `params` and the profile string outlive the call, which copies what it needs.
    let rc = unsafe { WlanConnect(h.0, &guid, &params, None) };
    if rc != 0 {
        return Err(WifiError::ConnectFailed {
            code: rc,
            profile: profile.clone(),
        });
    }

    // WlanConnect is asynchronous: it returns as soon as the request is queued.
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok((_, s)) = first_interface(&h) {
            if s == wlan_interface_state_connected {
                return Ok(profile);
            }
        }
    }
    Err(WifiError::Timeout {
        profile,
        waited: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_stops_at_the_nul() {
        let mut buf = [0u16; 8];
        for (i, c) in "Wi-Fi".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(utf16_to_string(&buf), "Wi-Fi");
        // No NUL at all: use the whole buffer rather than reading past it.
        let full: Vec<u16> = "abcd".encode_utf16().collect();
        assert_eq!(utf16_to_string(&full), "abcd");
        assert_eq!(utf16_to_string(&[]), "");
    }

    #[test]
    fn choose_profile_requires_the_network_to_be_in_range() {
        // The real failure this fixes: 31 saved networks, the first one a phone hotspot that is
        // not broadcasting. Preference order alone picks it and the association times out.
        let saved: Vec<String> = ["MyPhone", "OtherNet", "HomeNet"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let visible = vec![("HomeNet".to_string(), 80)];
        assert_eq!(
            choose_profile(None, &saved, &visible).as_deref(),
            Some("HomeNet")
        );
    }

    #[test]
    fn choose_profile_takes_the_strongest_in_range_not_the_most_preferred() {
        // Observed for real: preference order picked OtherNet over the network the machine
        // actually sits next to, purely because it was joined earlier.
        let saved: Vec<String> = ["OtherNet", "HomeNet"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let visible = vec![
            ("OtherNet".to_string(), 42),
            ("HomeNet".to_string(), 88),
        ];
        assert_eq!(
            choose_profile(None, &saved, &visible).as_deref(),
            Some("HomeNet")
        );
    }

    #[test]
    fn choose_profile_ignores_strong_networks_we_have_no_profile_for() {
        let saved = vec!["Home".to_string()];
        let visible = vec![("Neighbour".to_string(), 99), ("Home".to_string(), 30)];
        assert_eq!(choose_profile(None, &saved, &visible).as_deref(), Some("Home"));
    }

    #[test]
    fn choose_profile_prefers_the_hint_when_it_is_in_range() {
        let saved: Vec<String> = ["A", "B"].iter().map(|s| s.to_string()).collect();
        let visible = vec![("A".to_string(), 90), ("B".to_string(), 20)];
        // The hint wins even though A is far stronger: stability beats churn.
        assert_eq!(choose_profile(Some("B"), &saved, &visible).as_deref(), Some("B"));
    }

    #[test]
    fn choose_profile_ignores_a_hint_that_is_out_of_range() {
        // Coming home with a saved office network as the hint must not strand the switch.
        let saved: Vec<String> = ["Office", "Home"].iter().map(|s| s.to_string()).collect();
        let visible = vec![("Home".to_string(), 70)];
        assert_eq!(
            choose_profile(Some("Office"), &saved, &visible).as_deref(),
            Some("Home")
        );
    }

    #[test]
    fn choose_profile_ignores_a_hint_we_have_no_profile_for() {
        let saved = vec!["Home".to_string()];
        let visible = vec![("Home".to_string(), 70), ("Neighbour".to_string(), 90)];
        assert_eq!(
            choose_profile(Some("Neighbour"), &saved, &visible).as_deref(),
            Some("Home")
        );
    }

    #[test]
    fn choose_profile_gives_up_when_nothing_saved_is_in_range() {
        let saved: Vec<String> = ["A", "B"].iter().map(|s| s.to_string()).collect();
        let visible = vec![("SomeoneElse".to_string(), 90)];
        assert_eq!(choose_profile(None, &saved, &visible), None);
        assert_eq!(choose_profile(Some("A"), &saved, &visible), None);
        // And with the radio seeing nothing at all.
        assert_eq!(choose_profile(None, &saved, &[]), None);
    }

    #[test]
    fn every_error_has_a_distinct_actionable_message() {
        let errs = [
            WifiError::ServiceUnavailable(5),
            WifiError::NoAdapter,
            WifiError::NotReady,
            WifiError::RadioOff { hardware: true },
            WifiError::RadioOff { hardware: false },
            WifiError::NoProfile,
            WifiError::NoneInRange {
                saved: 31,
                visible: 4,
            },
            WifiError::ConnectFailed {
                code: 1168,
                profile: "HomeNet".into(),
            },
            WifiError::Timeout {
                profile: "HomeNet".into(),
                waited: Duration::from_secs(12),
            },
        ];
        let msgs: Vec<String> = errs.iter().map(|e| e.user_message()).collect();
        for m in &msgs {
            assert!(!m.is_empty(), "every error needs a message");
        }
        let mut uniq = msgs.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), msgs.len(), "messages must not collapse together");
    }

    #[test]
    fn hardware_radio_off_is_reported_differently_from_airplane_mode() {
        // The user can fix one of these from the Windows UI and not the other.
        assert_ne!(
            WifiError::RadioOff { hardware: true }.user_message(),
            WifiError::RadioOff { hardware: false }.user_message()
        );
    }
}
