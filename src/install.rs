//! Install and uninstall.
//!
//! Install is the only step that shows a UAC prompt. It copies the executable somewhere a
//! medium-integrity process cannot rewrite it, locks down the machine data directory, records
//! which adapters to manage, and registers the scheduled tasks that let the widget switch
//! without prompting again.
//!
//! Uninstall's first job is to put the machine back. It restores every metric from the journal
//! -- exact prior values, never guessed defaults -- before removing anything, so a user who
//! uninstalls mid-switch does not keep a parked Ethernet adapter forever.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, AdapterRef, MachineConfig, Mode};
use crate::lslog;
use crate::net::{wcm, Snapshot};
use crate::{elevate, tasks};

/// Do not flash a console window when shelling out from a GUI process.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "LinkSwitch";

pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    base.join("LinkSwitch")
}

pub fn installed_exe() -> PathBuf {
    install_dir().join("linkswitch.exe")
}

/// Is LinkSwitch set up on this machine, and pointing at a binary that still exists?
pub fn is_installed() -> bool {
    match tasks::health() {
        Ok(h) => {
            h.iter().all(|t| t.registered)
                && h.iter()
                    .filter_map(|t| t.command.as_deref())
                    .all(|c| Path::new(c).exists())
        }
        Err(_) => false,
    }
}

#[derive(Debug)]
pub struct InstallOutcome {
    pub ok: bool,
    pub messages: Vec<String>,
}

pub fn install(keep_wifi: bool, autostart: bool) -> InstallOutcome {
    let mut msgs = Vec::new();
    let mut ok = true;

    if !elevate::is_elevated() {
        return InstallOutcome {
            ok: false,
            messages: vec!["Install needs administrator rights.".into()],
        };
    }

    // 1. Copy the executable somewhere a medium-integrity process cannot rewrite it. Pointing a
    //    task that runs elevated at a binary the user can replace would hand any process running
    //    as that user a way to run code as administrator.
    let src = match elevate::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return InstallOutcome {
                ok: false,
                messages: vec![format!("Cannot locate the running executable: {e}")],
            }
        }
    };
    let dest = installed_exe();
    if src != dest {
        if let Err(e) = std::fs::create_dir_all(install_dir()) {
            return InstallOutcome {
                ok: false,
                messages: vec![format!("Cannot create {}: {e}", install_dir().display())],
            };
        }
        match std::fs::copy(&src, &dest) {
            Ok(_) => msgs.push(format!("Installed to {}", dest.display())),
            Err(e) => {
                // A running widget holds a lock on its own exe. Say so plainly.
                return InstallOutcome {
                    ok: false,
                    messages: vec![format!(
                        "Cannot copy to {}: {e}. Close LinkSwitch if it is running, then try again.",
                        dest.display()
                    )],
                };
            }
        }
    }

    // 2. Machine data directory, locked down. The elevated worker reads its configuration from
    //    here, so a standard user must not be able to rewrite it.
    let dir = config::machine_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return InstallOutcome {
            ok: false,
            messages: vec![format!("Cannot create {}: {e}", dir.display())],
        };
    }
    if let Err(e) = harden_dacl(&dir) {
        ok = false;
        msgs.push(format!("Warning: could not lock down {}: {e}", dir.display()));
    }

    // 3. Record the adapters to manage, keeping any existing choice.
    let snap = Snapshot::read();
    let (eth, wifi) = snap.candidates();
    let mut cfg = config::load_machine();
    if cfg.ethernet.is_none() {
        cfg.ethernet = eth.first().map(|n| AdapterRef::from(*n));
    }
    if cfg.wifi.is_none() {
        cfg.wifi = wifi.first().map(|n| AdapterRef::from(*n));
    }
    describe_choice(&mut msgs, &cfg);

    // 4. Optional, opt-in: stop Windows blocking Wi-Fi while Ethernet is up.
    if keep_wifi {
        let state = wcm::effective();
        if state.is_group_policy {
            msgs.push(
                "Skipped --keep-wifi-connected: the policy is set by Group Policy and a local \
                 change would be reverted at the next update."
                    .into(),
            );
        } else {
            if cfg.wcm_backup.is_none() {
                cfg.wcm_backup = Some(wcm::backup());
            }
            match wcm::set_allow() {
                Ok(()) => msgs.push(
                    "Windows will now keep Wi-Fi connected alongside Ethernet. This is a \
                     machine-wide policy; --uninstall puts it back exactly as it was."
                        .into(),
                ),
                Err(e) => {
                    ok = false;
                    msgs.push(format!("Could not change the Wi-Fi policy: {}", e.0));
                }
            }
        }
    }

    if let Err(e) = config::save_machine(&cfg) {
        ok = false;
        msgs.push(format!("Could not write the configuration: {e}"));
    }

    // 5. The scheduled tasks. This is what removes the per-click UAC prompt.
    let user = tasks::current_user();
    match tasks::register_all(&dest, &user) {
        Ok(()) => msgs.push(format!(
            "Registered {} scheduled tasks for {user}.",
            tasks::ALL_TASKS.len()
        )),
        Err(e) => {
            ok = false;
            msgs.push(format!("Could not register the scheduled tasks: {e}"));
        }
    }

    // 6. Start the widget at logon. A per-user Run entry, not a task: it needs no privileges and
    //    the user can see and remove it from Task Manager's Startup tab like any other app.
    if autostart {
        match set_autostart(Some(&dest)) {
            Ok(()) => msgs.push("The widget will start at logon.".into()),
            Err(e) => msgs.push(format!("Warning: could not set autostart: {e}")),
        }
    }

    InstallOutcome { ok, messages: msgs }
}

fn describe_choice(msgs: &mut Vec<String>, cfg: &MachineConfig) {
    match (&cfg.ethernet, &cfg.wifi) {
        (Some(e), Some(w)) => msgs.push(format!(
            "Managing \"{}\" and \"{}\".",
            if e.name.is_empty() { &e.description } else { &e.name },
            if w.name.is_empty() { &w.description } else { &w.name }
        )),
        (None, _) => msgs.push("Warning: no Ethernet adapter was found.".into()),
        (_, None) => msgs.push("Warning: no Wi-Fi adapter was found.".into()),
    }
}

pub fn uninstall() -> InstallOutcome {
    let mut msgs = Vec::new();
    let mut ok = true;

    if !elevate::is_elevated() {
        return InstallOutcome {
            ok: false,
            messages: vec!["Uninstall needs administrator rights.".into()],
        };
    }

    // Put the machine back FIRST. Everything after this is cleanup, and a failure there must not
    // leave an adapter parked at metric 9000 with nothing left to fix it.
    let report = crate::apply::apply(Mode::Auto);
    msgs.push(report.message.clone());
    if !report.ok {
        ok = false;
    }

    let cfg = config::load_machine();
    if let Some(backup) = cfg.wcm_backup {
        match wcm::restore(backup) {
            Ok(()) => msgs.push("Restored the Windows Wi-Fi connection policy.".into()),
            Err(e) => {
                ok = false;
                msgs.push(format!("Could not restore the Wi-Fi policy: {}", e.0));
            }
        }
    }

    match tasks::unregister_all() {
        Ok(()) => msgs.push("Removed the scheduled tasks.".into()),
        Err(e) => {
            ok = false;
            msgs.push(format!("Could not remove the scheduled tasks: {e}"));
        }
    }

    if let Err(e) = set_autostart(None) {
        msgs.push(format!("Warning: could not clear autostart: {e}"));
    }

    // Leave the log behind on failure: it is the only record of what went wrong.
    if ok {
        let _ = std::fs::remove_dir_all(config::machine_dir());
        let _ = std::fs::remove_dir_all(config::user_dir());
        msgs.push("Removed LinkSwitch's data.".into());
    } else {
        msgs.push(format!(
            "Left {} in place so the log can be inspected.",
            config::machine_dir().display()
        ));
    }

    msgs.push(format!(
        "The program itself is still at {}; delete that folder to finish.",
        install_dir().display()
    ));

    InstallOutcome { ok, messages: msgs }
}

/// Restrict the machine data directory to Administrators and SYSTEM, with read-only access for
/// ordinary users.
///
/// `icacls` rather than `SetNamedSecurityInfoW`: this runs once, elevated, and a single
/// auditable command line is easier for a reader of an open-source security-sensitive tool to
/// check than fifty lines of ACL construction.
fn harden_dacl(dir: &Path) -> std::io::Result<()> {
    let out = Command::new("icacls")
        .arg(dir)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F", // Administrators
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F", // SYSTEM
            "/grant:r",
            "*S-1-5-32-545:(OI)(CI)RX", // Users: read only
            "/T",
            "/C",
            "/Q",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// Add or remove the per-user startup entry.
fn set_autostart(exe: Option<&Path>) -> std::io::Result<()> {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, NO_ERROR, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
    };

    let subkey = HSTRING::from(RUN_KEY);
    let value = w!("LinkSwitch");
    debug_assert_eq!(RUN_VALUE, "LinkSwitch");

    match exe {
        Some(p) => {
            // Quote the path: Program Files contains a space, and an unquoted Run entry with a
            // space is a classic unquoted-path problem.
            let cmd = HSTRING::from(format!("\"{}\"", p.display()));
            // Length in BYTES including the terminating NUL, which is what REG_SZ wants.
            let cb = ((cmd.len() + 1) * std::mem::size_of::<u16>()) as u32;
            // SAFETY: `cmd` outlives the call and `cb` includes the terminating NUL that HSTRING
            // guarantees.
            let rc = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(subkey.as_ptr()),
                    value,
                    REG_SZ.0,
                    Some(cmd.as_ptr() as *const core::ffi::c_void),
                    cb,
                )
            };
            if WIN32_ERROR(rc.0) == NO_ERROR {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(rc.0 as i32))
            }
        }
        None => {
            // SAFETY: deleting a named value under a fixed key.
            let rc =
                unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()), value) };
            let rc = WIN32_ERROR(rc.0);
            if rc == NO_ERROR || rc == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(rc.0 as i32))
            }
        }
    }
}

/// Relaunch elevated to run an install-class verb, showing one UAC prompt.
pub fn self_elevate(args: &[&str]) -> Result<u32, String> {
    let exe = elevate::current_exe().map_err(|e| e.to_string())?;
    match elevate::relaunch_elevated(&exe, args) {
        Ok(code) => Ok(code),
        Err(elevate::ElevateError::Declined) => {
            Err("Administrator rights are needed, and the request was declined.".into())
        }
        Err(elevate::ElevateError::Failed(e)) => Err(e),
    }
}

/// Ask the worker to apply a mode, preferring the no-prompt path.
///
/// Normally this starts the pre-registered scheduled task, which supplies an elevated token with
/// no prompt. When the tasks are missing -- LinkSwitch was never installed, or someone deleted
/// them -- it falls back to relaunching elevated, which prompts. Prompting is worse but working
/// is better than not working.
pub fn request_apply(mode: Mode) -> Result<(), String> {
    match tasks::run(mode) {
        Ok(()) => Ok(()),
        Err(task_err) => {
            lslog!("scheduled task unavailable ({task_err}); falling back to a UAC prompt");
            let code = self_elevate(&["--apply", mode.as_str()])?;
            if code == crate::apply::exit::OK {
                Ok(())
            } else {
                Err(format!("The switch failed (exit code {code}). {task_err}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_paths_sit_under_program_files_and_programdata() {
        // The worker's binary and configuration must be somewhere a standard user cannot
        // rewrite, or the elevated task becomes a way to run arbitrary code as administrator.
        let exe = installed_exe().to_string_lossy().to_lowercase();
        assert!(exe.contains("linkswitch.exe"));
        assert!(exe.contains("program files"), "got {exe}");

        let data = config::machine_dir().to_string_lossy().to_lowercase();
        assert!(data.contains("programdata"), "got {data}");
    }

    #[test]
    fn user_data_is_separate_from_machine_data() {
        assert_ne!(config::user_dir(), config::machine_dir());
    }

    #[test]
    fn the_autostart_value_name_is_stable() {
        // Changing this would orphan the old Run entry and start LinkSwitch twice.
        assert_eq!(RUN_VALUE, "LinkSwitch");
        assert!(RUN_KEY.ends_with(r"CurrentVersion\Run"));
    }
}
