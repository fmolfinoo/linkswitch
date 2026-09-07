//! Win32 error helpers for the IP Helper layer.
//!
//! `GetIpInterfaceEntry`, `SetIpInterfaceEntry`, `GetIpInterfaceTable`, `GetIpForwardTable2`,
//! `GetIfEntry2` and the `Notify*Change` family all return a bare [`WIN32_ERROR`], not a
//! `windows::core::Result`. Never route these through `.ok()?` and then compare the resulting
//! `windows::core::Error` against a `WIN32_ERROR` constant: `.ok()` maps 5 to HRESULT
//! `0x80070005`, so every such comparison is silently false forever.

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND, NO_ERROR,
    WIN32_ERROR,
};

/// The interface exists, but this address family is not bound to it.
///
/// This is a completely normal condition, not a failure. On a machine where a VPN has applied
/// IPv6 leak protection by unbinding `ms_tcpip6`, every physical adapter answers `ERROR_NOT_FOUND`
/// for `AF_INET6`.
#[inline]
pub fn is_family_absent(rc: WIN32_ERROR) -> bool {
    rc == ERROR_NOT_FOUND
}

/// The LUID does not resolve to any interface: the adapter was unplugged, uninstalled, or
/// disabled in Device Manager. Windows cannot distinguish those three, and neither can we.
#[inline]
pub fn is_adapter_gone(rc: WIN32_ERROR) -> bool {
    rc == ERROR_FILE_NOT_FOUND
}

/// Human-readable explanation used in the log and surfaced in the widget.
pub fn explain(rc: WIN32_ERROR) -> &'static str {
    match rc {
        NO_ERROR => "ok",
        ERROR_ACCESS_DENIED => {
            "access denied - the process is not elevated (the scheduled task did not supply an \
             elevated token)"
        }
        ERROR_FILE_NOT_FOUND => {
            "adapter no longer present - it was unplugged, disabled, or its driver was replaced"
        }
        ERROR_NOT_FOUND => "address family not bound on this adapter - nothing to do",
        ERROR_INVALID_PARAMETER => {
            "invalid parameter - Family was unset/AF_UNSPEC, or SitePrefixLength was not reset \
             to 0 on an IPv4 Set"
        }
        _ => "unexpected Win32 error",
    }
}

/// `explain`, but carrying the numeric code for logs and bug reports.
pub fn describe(rc: WIN32_ERROR) -> String {
    format!("{} (Win32 {})", explain(rc), rc.0)
}
