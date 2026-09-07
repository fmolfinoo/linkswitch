//! Elevation: detecting it, and asking for it.
//!
//! LinkSwitch is one binary in two roles. The widget runs unelevated all day; the worker needs
//! administrator rights to write an interface metric. The normal path gets those rights from a
//! pre-registered scheduled task, so there is no UAC prompt per click. This module covers the
//! two places that is not enough: knowing whether we already have rights, and the fallback that
//! relaunches ourselves with a prompt when the tasks are not installed.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Are we running with an elevated token right now?
///
/// This asks the token, not the group list. Under UAC's Admin Approval Mode an administrator's
/// unelevated process still *contains* the Administrators SID -- marked deny-only -- so a
/// group-membership check reports "yes, admin" for a process that cannot write a single
/// interface metric. Measured on the development machine:
/// `BUILTIN\Administrators  Alias  S-1-5-32-544  Group used for deny only`.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; `token` is a
    // valid out-param.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0u32;
    // SAFETY: `elevation` is the correct payload type and size for TokenElevation.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    }
    .is_ok();
    // SAFETY: `token` came from OpenProcessToken and is closed exactly once.
    unsafe {
        let _ = CloseHandle(token);
    }
    ok && elevation.TokenIsElevated != 0
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Quote one argument for a Windows command line.
///
/// Paths reach here from `%ProgramFiles%` and from wherever the user unzipped the build, so
/// spaces are the norm rather than the exception.
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::with_capacity(a.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Backslashes immediately before a quote must be doubled, then the quote escaped.
                for _ in 0..=backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Trailing backslashes would otherwise escape the closing quote.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

pub fn join_args(args: &[&str]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug)]
pub enum ElevateError {
    /// The user clicked "No" on the UAC prompt, or policy blocked it.
    Declined,
    Failed(String),
}

/// Relaunch this executable elevated with the given arguments and wait for it to finish.
///
/// Returns the child's exit code. This is the fallback path: it shows a UAC prompt every time,
/// which is exactly what the scheduled tasks exist to avoid, so it is used only for `--install`
/// and when the tasks are missing.
pub fn relaunch_elevated(exe: &Path, args: &[&str]) -> Result<u32, ElevateError> {
    let file = wide(&exe.to_string_lossy());
    let params = wide(&join_args(args));
    let verb = wide("runas");

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY: `info` is fully initialised with cbSize set, and every PCWSTR points at a NUL-
    // terminated buffer that outlives the call.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            // ERROR_CANCELLED (1223) is the user declining the prompt, not a malfunction.
            return Err(if e.code().0 as u32 == 0x800704C7 {
                ElevateError::Declined
            } else {
                ElevateError::Failed(e.to_string())
            });
        }
    }

    if info.hProcess.is_invalid() {
        return Err(ElevateError::Failed("no child process handle".into()));
    }

    // SAFETY: `hProcess` is a live process handle owned by us thanks to SEE_MASK_NOCLOSEPROCESS.
    let code = unsafe {
        let waited = WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        if waited == WAIT_OBJECT_0 {
            let _ = GetExitCodeProcess(info.hProcess, &mut code);
        }
        let _ = CloseHandle(info.hProcess);
        code
    };
    Ok(code)
}

/// The path of the running executable.
pub fn current_exe() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_arguments_are_not_quoted() {
        assert_eq!(quote_arg("--apply"), "--apply");
        assert_eq!(quote_arg("wifi"), "wifi");
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        assert_eq!(
            quote_arg(r"C:\Program Files\LinkSwitch\linkswitch.exe"),
            r#""C:\Program Files\LinkSwitch\linkswitch.exe""#
        );
    }

    #[test]
    fn trailing_backslashes_do_not_escape_the_closing_quote() {
        // The classic Windows command-line bug: "C:\dir\" would swallow the quote.
        assert_eq!(quote_arg(r"C:\some dir\"), r#""C:\some dir\\""#);
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn empty_argument_survives_as_an_empty_pair_of_quotes() {
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn arguments_join_with_single_spaces() {
        assert_eq!(
            join_args(&["--apply", "wifi", r"C:\Program Files\x"]),
            r#"--apply wifi "C:\Program Files\x""#
        );
    }
}
