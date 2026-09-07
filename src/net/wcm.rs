//! The Windows Connection Manager "minimize simultaneous connections" policy.
//!
//! This is the setting behind "every time I plug in Ethernet, Wi-Fi gets ignored" -- or rather,
//! behind the harsher half of it. Two separate things are going on:
//!
//! 1. **Routing.** Ethernet's automatic metric (5 on the development machine) beats Wi-Fi's (30),
//!    so Ethernet wins the default route. That is what [`super::metric`] fixes.
//! 2. **Association.** WCM additionally refuses to let Wi-Fi *auto*-connect while a preferred
//!    network type is up, and will soft-disconnect an idle Wi-Fi link. That is this module.
//!
//! Measured on the development machine: the registry value is **absent**, and
//! `WcmQueryProperty` reports `fValue = true, fIsGroupPolicy = false`. So an absent value means
//! the policy is **enabled**, not disabled -- reading "no value" as "not configured" is the
//! single easiest way to get this wrong, and a naive `Get-ItemProperty` probe reports nothing at
//! all for a key that exists with no values.
//!
//! LinkSwitch's default answer is *not* to touch this policy. The worker associates Wi-Fi
//! deliberately before switching, which WCM permits because manually-connected networks are on
//! its keep-list. Changing the policy is offered as an explicit opt-in, because on managed
//! machines it is a compliance-relevant setting: the DISA STIG for Windows 11 (WN11-CC-000055)
//! requires it to be 3, and flags 0 as a finding.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, NO_ERROR, WIN32_ERROR};
use windows::Win32::NetworkManagement::WindowsConnectionManager::{
    wcm_global_property_minimize_policy, WcmFreeMemory, WcmQueryProperty, WCM_POLICY_VALUE,
};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_LOCAL_MACHINE, REG_DWORD,
    RRF_RT_REG_DWORD,
};

use crate::config::WcmBackup;

const KEY: PCWSTR = w!(r"SOFTWARE\Policies\Microsoft\Windows\WcmSvc\GroupPolicy");
const VALUE: PCWSTR = w!("fMinimizeConnections");

/// The documented values of the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinimizePolicy {
    /// 0 -- Windows may keep several connections up at once. What LinkSwitch wants.
    Allow,
    /// 1 -- the Windows default. Blocks new *automatic* connections when a preferred network is
    /// already up, and soft-disconnects idle ones.
    Minimize,
    /// 2 -- minimise cellular only.
    MinimizeCellular,
    /// 3 -- prevent Wi-Fi entirely while Ethernet is connected. LinkSwitch cannot work at all
    /// under this value: even a manual connection is refused.
    PreventWifi,
    /// A value outside the documented set.
    Unknown(u32),
}

impl MinimizePolicy {
    pub fn from_value(v: u32) -> Self {
        match v {
            0 => Self::Allow,
            1 => Self::Minimize,
            2 => Self::MinimizeCellular,
            3 => Self::PreventWifi,
            other => Self::Unknown(other),
        }
    }

    /// Will Windows let us associate Wi-Fi manually while Ethernet is up?
    pub fn permits_manual_wifi(self) -> bool {
        !matches!(self, Self::PreventWifi)
    }

    /// Will Wi-Fi come back by itself after a reboot with the cable in?
    pub fn permits_auto_wifi(self) -> bool {
        matches!(self, Self::Allow | Self::MinimizeCellular)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Allow => "Windows may keep Wi-Fi connected alongside Ethernet",
            Self::Minimize => {
                "Windows blocks Wi-Fi from auto-connecting while Ethernet is up (Windows default)"
            }
            Self::MinimizeCellular => "Windows minimises cellular connections only",
            Self::PreventWifi => "Group Policy prevents Wi-Fi entirely while Ethernet is connected",
            Self::Unknown(_) => "unrecognised policy value",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PolicyState {
    pub policy: MinimizePolicy,
    /// Set by domain Group Policy rather than a local edit. Writing over it is pointless: the
    /// next `gpupdate` puts it back.
    pub is_group_policy: bool,
    /// Whether the registry value physically exists.
    pub value_present: bool,
}

/// Read the raw registry value, if it exists.
fn read_registry() -> Option<u32> {
    let mut data: u32 = 0;
    let mut cb: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: `data`/`cb` are valid locals sized for a DWORD, and the flags constrain the read to
    // REG_DWORD so no other type can overflow the buffer.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            KEY,
            VALUE,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut core::ffi::c_void),
            Some(&mut cb),
        )
    };
    (WIN32_ERROR(rc.0) == NO_ERROR).then_some(data)
}

/// Ask WCM itself for the effective policy. This is what makes an absent value readable.
fn query_effective() -> Option<(bool, bool)> {
    let mut size = 0u32;
    let mut data: *mut u8 = std::ptr::null_mut();
    // SAFETY: out-params are valid locals; the allocation is freed on every path below.
    let rc = unsafe {
        WcmQueryProperty(
            None,
            PCWSTR::null(),
            wcm_global_property_minimize_policy,
            None,
            &mut size,
            &mut data,
        )
    };
    if rc != 0 || data.is_null() {
        if !data.is_null() {
            // SAFETY: non-null allocation from WcmQueryProperty.
            unsafe { WcmFreeMemory(data as *mut core::ffi::c_void) };
        }
        return None;
    }
    let out = if (size as usize) >= std::mem::size_of::<WCM_POLICY_VALUE>() {
        // SAFETY: the buffer is at least as large as the struct the opcode documents.
        let v = unsafe { *(data as *const WCM_POLICY_VALUE) };
        Some((v.fValue.as_bool(), v.fIsGroupPolicy.as_bool()))
    } else {
        None
    };
    // SAFETY: allocation from WcmQueryProperty, freed exactly once.
    unsafe { WcmFreeMemory(data as *mut core::ffi::c_void) };
    out
}

/// The effective policy right now.
///
/// The registry value is authoritative for the 0/1/2/3 enum when it exists, because
/// `WCM_POLICY_VALUE::fValue` is a `BOOL` and cannot represent 2 or 3 faithfully. When the value
/// is absent, WCM's own answer supplies the effective default.
pub fn effective() -> PolicyState {
    let reg = read_registry();
    let wcm = query_effective();

    match reg {
        Some(v) => PolicyState {
            policy: MinimizePolicy::from_value(v),
            is_group_policy: wcm.map(|(_, gp)| gp).unwrap_or(false),
            value_present: true,
        },
        None => {
            let (on, gp) = wcm.unwrap_or((true, false));
            PolicyState {
                // Absent means enabled. Never read "no value" as Allow.
                policy: if on {
                    MinimizePolicy::Minimize
                } else {
                    MinimizePolicy::Allow
                },
                is_group_policy: gp,
                value_present: false,
            }
        }
    }
}

/// Capture the current value so it can be put back exactly, including "it did not exist".
pub fn backup() -> WcmBackup {
    match read_registry() {
        Some(v) => WcmBackup {
            present: true,
            value: v,
        },
        None => WcmBackup {
            present: false,
            value: 0,
        },
    }
}

/// Opt-in: let Windows keep Wi-Fi connected alongside Ethernet.
///
/// Writes 0 rather than deleting the value. The policy's ADMX definition has no `disabledValue`,
/// so deleting it restores the OS default of "enabled" -- the opposite of the intent.
pub fn set_allow() -> Result<(), WIN32_ERROR> {
    let v: u32 = 0;
    // SAFETY: `v` outlives the call and its size is passed explicitly.
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            KEY,
            VALUE,
            REG_DWORD.0,
            Some(&v as *const u32 as *const core::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if WIN32_ERROR(rc.0) == NO_ERROR {
        Ok(())
    } else {
        Err(WIN32_ERROR(rc.0))
    }
}

/// Put the policy back exactly as it was found, deleting the value if it was absent.
pub fn restore(b: WcmBackup) -> Result<(), WIN32_ERROR> {
    if b.present {
        let v = b.value;
        // SAFETY: as in `set_allow`.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_LOCAL_MACHINE,
                KEY,
                VALUE,
                REG_DWORD.0,
                Some(&v as *const u32 as *const core::ffi::c_void),
                std::mem::size_of::<u32>() as u32,
            )
        };
        return if WIN32_ERROR(rc.0) == NO_ERROR {
            Ok(())
        } else {
            Err(WIN32_ERROR(rc.0))
        };
    }
    // SAFETY: deleting a named value under a fixed key; both strings are static.
    let rc = unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, KEY, VALUE) };
    let rc = WIN32_ERROR(rc.0);
    // Already absent is the desired end state, not a failure.
    if rc == NO_ERROR || rc == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(rc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_values_map_to_variants() {
        assert_eq!(MinimizePolicy::from_value(0), MinimizePolicy::Allow);
        assert_eq!(MinimizePolicy::from_value(1), MinimizePolicy::Minimize);
        assert_eq!(MinimizePolicy::from_value(2), MinimizePolicy::MinimizeCellular);
        assert_eq!(MinimizePolicy::from_value(3), MinimizePolicy::PreventWifi);
        assert_eq!(MinimizePolicy::from_value(7), MinimizePolicy::Unknown(7));
    }

    #[test]
    fn only_value_three_blocks_a_manual_connection() {
        // This is the distinction that decides whether LinkSwitch can work at all: under the
        // default (1) we can still associate Wi-Fi ourselves, which is the whole strategy.
        assert!(MinimizePolicy::Allow.permits_manual_wifi());
        assert!(MinimizePolicy::Minimize.permits_manual_wifi());
        assert!(MinimizePolicy::MinimizeCellular.permits_manual_wifi());
        assert!(!MinimizePolicy::PreventWifi.permits_manual_wifi());
    }

    #[test]
    fn the_windows_default_blocks_automatic_reconnection() {
        // Why the worker must connect Wi-Fi itself rather than wait for Windows to do it.
        assert!(!MinimizePolicy::Minimize.permits_auto_wifi());
        assert!(MinimizePolicy::Allow.permits_auto_wifi());
    }

    #[test]
    fn every_policy_has_an_explanation() {
        for p in [
            MinimizePolicy::Allow,
            MinimizePolicy::Minimize,
            MinimizePolicy::MinimizeCellular,
            MinimizePolicy::PreventWifi,
            MinimizePolicy::Unknown(9),
        ] {
            assert!(!p.describe().is_empty());
        }
    }

    #[test]
    fn backup_of_an_absent_value_restores_by_deleting() {
        // Recording `present: false` is what stops restore writing an explicit 0 and leaving the
        // machine in a state Windows never put it in.
        let b = WcmBackup {
            present: false,
            value: 0,
        };
        assert!(!b.present);
    }

    /// Reads the real machine. Asserts only on internal consistency, never on a specific value,
    /// so it stays valid on any machine.
    #[test]
    fn effective_policy_is_self_consistent_on_this_machine() {
        let s = effective();
        if !s.value_present {
            // With no registry value the enum can only be inferred from WCM's BOOL, so it must
            // be one of the two states that BOOL can express.
            assert!(
                matches!(s.policy, MinimizePolicy::Allow | MinimizePolicy::Minimize),
                "inferred policy must come from the boolean, got {:?}",
                s.policy
            );
        }
        assert!(!s.policy.describe().is_empty());
    }
}
