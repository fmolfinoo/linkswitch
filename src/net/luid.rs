//! Stable interface identity.
//!
//! `NET_LUID_LH` is a C union (`{ Value: u64, Info: NET_LUID_LH_0 }`) and windows-rs derives
//! neither `PartialEq`, `Debug` nor `Hash` for it, so comparing two of them directly is a hard
//! compile error. Everything in LinkSwitch therefore stores and compares the plain `u64`.
//!
//! The LUID is also the identity we persist. `IfIndex` is explicitly documented as not
//! persistent -- it changes across reboots and re-enumeration -- so a config keyed on it would
//! silently start steering the wrong adapter. The LUID is stable for as long as the adapter's
//! driver instance exists, which is the best Windows offers. It does still change when a NIC is
//! reinstalled or a dock is swapped, so [`crate::config`] additionally records the adapter's
//! friendly name and description to recover gracefully. See `LuidKey::is_stale`.

use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;

/// The `Value` field of a `NET_LUID_LH`, usable as a map key, comparable, and serialisable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LuidKey(pub u64);

impl LuidKey {
    #[inline]
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for LuidKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl From<NET_LUID_LH> for LuidKey {
    #[inline]
    fn from(l: NET_LUID_LH) -> Self {
        // SAFETY: reading the `Value` arm of the union is always valid -- it is the full 64-bit
        // storage, and every bit pattern is a legal u64.
        LuidKey(unsafe { l.Value })
    }
}

impl From<LuidKey> for NET_LUID_LH {
    #[inline]
    fn from(k: LuidKey) -> Self {
        NET_LUID_LH { Value: k.0 }
    }
}

#[inline]
pub fn luid_u64(l: NET_LUID_LH) -> u64 {
    // SAFETY: see `From<NET_LUID_LH>`.
    unsafe { l.Value }
}

#[inline]
pub fn luid_from_u64(v: u64) -> NET_LUID_LH {
    NET_LUID_LH { Value: v }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luid_round_trips_through_u64() {
        for v in [0u64, 1, 0x0000_0000_0000_1234, u64::MAX, 0xDEAD_BEEF_CAFE_F00D] {
            let native = luid_from_u64(v);
            assert_eq!(luid_u64(native), v);
            assert_eq!(LuidKey::from(native), LuidKey(v));
            assert_eq!(luid_u64(NET_LUID_LH::from(LuidKey(v))), v);
        }
    }

    #[test]
    fn zero_luid_is_recognised() {
        assert!(LuidKey(0).is_zero());
        assert!(!LuidKey(1).is_zero());
    }
}
