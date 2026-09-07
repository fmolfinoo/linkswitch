//! Scheduled tasks: how the unelevated widget performs an administrator-only action without a
//! UAC prompt on every click.
//!
//! # The mechanism
//!
//! A task registered with `RunLevel = HighestAvailable` and a principal of the *installing user*
//! runs elevated when started. Windows lets a process start a task that its own user owns, so
//! the medium-integrity widget can call `Run` on a task it registered while elevated, and the
//! action runs with an elevated token and no prompt. Registering such a task is itself an
//! elevated operation, which is why `--install` prompts once and nothing else ever does.
//!
//! # Why four fixed tasks instead of one that takes an argument
//!
//! `schtasks /run` cannot pass arguments at all, and the COM `$(Arg0)` substitution that can is
//! the wrong shape here for a security reason: a task that runs *whatever it is told* with an
//! elevated token is a medium-to-high integrity bridge for every process running as that user.
//! Four fixed verbs have no injection surface -- the worst a hostile process can do is switch
//! the network between two states the user already asked for.
//!
//! The same reasoning drives the rest of the hardening: the exe lives under `%ProgramFiles%`
//! where a medium-integrity process cannot rewrite it, the worker reads its configuration only
//! from `%ProgramData%`, and no task ever points at `cmd.exe` or `powershell.exe`.

use windows::core::BSTR;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::TaskScheduler::{
    ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
};

use crate::config::Mode;

/// Task Scheduler folder holding everything LinkSwitch registers.
pub const FOLDER: &str = "LinkSwitch";

/// Every task LinkSwitch registers.
pub const ALL_TASKS: &[TaskSpec] = &[
    TaskSpec {
        name: "ApplyEthernet",
        args: "--apply ethernet",
        description: "LinkSwitch: send internet traffic over Ethernet.",
        logon_trigger: false,
    },
    TaskSpec {
        name: "ApplyWifi",
        args: "--apply wifi",
        description: "LinkSwitch: send internet traffic over Wi-Fi. Ethernet stays connected and \
                      link-up, and keeps serving its own subnet.",
        logon_trigger: false,
    },
    TaskSpec {
        name: "ApplyAuto",
        args: "--apply auto",
        description: "LinkSwitch: hand both adapters back to Windows' automatic metrics.",
        logon_trigger: false,
    },
    TaskSpec {
        name: "Recover",
        args: "--recover",
        description: "LinkSwitch: at logon, undo any half-finished change left by an interrupted \
                      switch.",
        logon_trigger: true,
    },
];

pub struct TaskSpec {
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
    /// Runs at logon rather than only on demand.
    pub logon_trigger: bool,
}

pub fn task_path(name: &str) -> String {
    format!(r"\{FOLDER}\{name}")
}

/// Escape text for an XML text node or attribute value.
///
/// Not optional: the substituted values are a Windows account name and an install path.
/// `Program Files (x86)`, a domain containing `&`, or an apostrophe in a user's name would
/// otherwise produce a malformed document and an opaque `RegisterTask` HRESULT.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Build the task XML.
///
/// Several `<Settings>` defaults are actively wrong for this application and are overridden:
///
/// * `DisallowStartIfOnBatteries` and `StopIfGoingOnBatteries` both default to **true**. Left
///   alone, the widget would silently do nothing on an unplugged laptop -- which is exactly when
///   someone reaches for a Wi-Fi switch.
/// * `MultipleInstancesPolicy` defaults to `IgnoreNew`, which drops a second click while the
///   first worker is still running.
/// * `ExecutionTimeLimit` defaults to 72 hours.
pub fn build_xml(spec: &TaskSpec, exe: &str, user: &str) -> String {
    let trigger = if spec.logon_trigger {
        format!(
            r#"    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT15S</Delay>
    </LogonTrigger>
"#,
            user = xml_escape(user)
        )
    } else {
        String::new()
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>LinkSwitch</Author>
    <URI>\{folder}\{name}</URI>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
{trigger}  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <MultipleInstancesPolicy>Queue</MultipleInstancesPolicy>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <Hidden>true</Hidden>
    <Enabled>true</Enabled>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT2M</ExecutionTimeLimit>
    <Priority>5</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>"#,
        folder = FOLDER,
        name = xml_escape(spec.name),
        description = xml_escape(spec.description),
        trigger = trigger,
        user = xml_escape(user),
        exe = xml_escape(exe),
        args = xml_escape(spec.args),
    )
}

/// The account tasks are registered for: `DOMAIN\User`.
pub fn current_user() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();
    if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    }
}

/// RAII COM apartment for one operation.
struct ComGuard;

impl ComGuard {
    fn new() -> Self {
        // SAFETY: paired with CoUninitialize in Drop. Called only on a dedicated worker thread,
        // never on the UI thread, so it cannot collide with winit's own apartment.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        Self
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// Run a Task Scheduler operation on its own short-lived thread with its own COM apartment.
///
/// The widget's thread already belongs to winit's apartment, and initialising COM differently
/// there is a good way to break the event loop in ways that only show up later.
fn with_service<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce(&ITaskService) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let _com = ComGuard::new();
        // SAFETY: CLSID_TaskScheduler is an in-process COM server; the apartment is initialised.
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| format!("cannot create the Task Scheduler service: {e}"))?;
        // SAFETY: connecting to the local scheduler as the current user.
        unsafe {
            service
                .Connect(
                    &VARIANT::default(),
                    &VARIANT::default(),
                    &VARIANT::default(),
                    &VARIANT::default(),
                )
                .map_err(|e| format!("cannot connect to the Task Scheduler: {e}"))?;
        }
        f(&service)
    })
    .join()
    .map_err(|_| "the Task Scheduler thread panicked".to_string())?
}

/// Register (or update) every task. Requires elevation.
pub fn register_all(exe: &std::path::Path, user: &str) -> Result<(), String> {
    let exe = exe.to_string_lossy().to_string();
    let user = user.to_string();
    with_service(move |service| {
        // SAFETY: BSTRs live for the duration of each call.
        unsafe {
            let root = service
                .GetFolder(&BSTR::from("\\"))
                .map_err(|e| format!("cannot open the root task folder: {e}"))?;

            // Create our folder if it is not already there.
            let folder = match root.GetFolder(&BSTR::from(FOLDER)) {
                Ok(f) => f,
                Err(_) => root
                    .CreateFolder(&BSTR::from(FOLDER), &VARIANT::default())
                    .map_err(|e| format!("cannot create the LinkSwitch task folder: {e}"))?,
            };

            for spec in ALL_TASKS {
                let xml = build_xml(spec, &exe, &user);
                folder
                    .RegisterTask(
                        &BSTR::from(spec.name),
                        &BSTR::from(xml.as_str()),
                        TASK_CREATE_OR_UPDATE.0,
                        &VARIANT::default(),
                        &VARIANT::default(),
                        TASK_LOGON_INTERACTIVE_TOKEN,
                        &VARIANT::default(),
                    )
                    .map_err(|e| format!("cannot register task {}: {e}", spec.name))?;
            }
        }
        Ok(())
    })
}

/// Remove every task and the folder. Requires elevation.
pub fn unregister_all() -> Result<(), String> {
    with_service(move |service| {
        // SAFETY: BSTRs live for the duration of each call.
        unsafe {
            let root = service
                .GetFolder(&BSTR::from("\\"))
                .map_err(|e| format!("cannot open the root task folder: {e}"))?;
            let Ok(folder) = root.GetFolder(&BSTR::from(FOLDER)) else {
                return Ok(()); // nothing registered
            };
            for spec in ALL_TASKS {
                let _ = folder.DeleteTask(&BSTR::from(spec.name), 0);
            }
            let _ = root.DeleteFolder(&BSTR::from(FOLDER), 0);
        }
        Ok(())
    })
}

/// What a registered task is pointing at, so the widget can notice a stale install.
#[derive(Debug, Clone)]
pub struct TaskHealth {
    pub name: String,
    pub registered: bool,
    pub command: Option<String>,
    pub last_result: Option<i32>,
}

/// Inspect every task. Unprivileged: reading tasks the current user owns needs no elevation.
pub fn health() -> Result<Vec<TaskHealth>, String> {
    with_service(move |service| {
        let mut out = Vec::new();
        // SAFETY: BSTRs live for the duration of each call.
        unsafe {
            let root = service
                .GetFolder(&BSTR::from("\\"))
                .map_err(|e| format!("cannot open the root task folder: {e}"))?;
            let folder = root.GetFolder(&BSTR::from(FOLDER)).ok();
            for spec in ALL_TASKS {
                let mut h = TaskHealth {
                    name: spec.name.to_string(),
                    registered: false,
                    command: None,
                    last_result: None,
                };
                if let Some(f) = folder.as_ref() {
                    if let Ok(task) = f.GetTask(&BSTR::from(spec.name)) {
                        h.registered = true;
                        h.last_result = task.LastTaskResult().ok();
                        // The XML is the reliable way to read back the action's command: walking
                        // the action collection needs several more interface casts for no gain.
                        if let Ok(xml) = task.Xml() {
                            let xml = xml.to_string();
                            h.command = extract_command(&xml);
                        }
                    }
                }
                out.push(h);
            }
        }
        Ok(out)
    })
}

/// Pull `<Command>...</Command>` out of a task's XML.
pub fn extract_command(xml: &str) -> Option<String> {
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = xml[start..].find("</Command>")? + start;
    Some(unescape(&xml[start..end]))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // `&amp;` last: doing it first would re-expand entities that came from literal text.
        .replace("&amp;", "&")
}

/// Start the task for a mode. Unprivileged; the task supplies the elevated token.
pub fn run(mode: Mode) -> Result<(), String> {
    let name = mode.task_name().to_string();
    with_service(move |service| {
        // SAFETY: BSTRs live for the duration of each call.
        unsafe {
            let root = service
                .GetFolder(&BSTR::from("\\"))
                .map_err(|e| format!("cannot open the root task folder: {e}"))?;
            let folder = root
                .GetFolder(&BSTR::from(FOLDER))
                .map_err(|_| "LinkSwitch is not installed yet.".to_string())?;
            let task = folder
                .GetTask(&BSTR::from(name.as_str()))
                .map_err(|_| format!("the {name} task is missing; reinstall LinkSwitch."))?;
            task.Run(&VARIANT::default())
                .map_err(|e| format!("cannot start the {name} task: {e}"))?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escapes_every_dangerous_character() {
        assert_eq!(xml_escape("a&b"), "a&amp;b");
        assert_eq!(xml_escape("<x>"), "&lt;x&gt;");
        assert_eq!(xml_escape("say \"hi\""), "say &quot;hi&quot;");
        assert_eq!(xml_escape("O'Brien"), "O&apos;Brien");
        assert_eq!(xml_escape("plain"), "plain");
    }

    #[test]
    fn a_domain_with_an_ampersand_does_not_break_the_document() {
        // Real-world shapes that would otherwise produce malformed XML and an opaque HRESULT.
        let xml = build_xml(
            &ALL_TASKS[0],
            r"C:\Program Files (x86)\Link&Switch\linkswitch.exe",
            r"R&D\O'Brien",
        );
        assert!(!xml.contains("Link&Switch"), "raw ampersand leaked into the XML");
        assert!(xml.contains("Link&amp;Switch"));
        assert!(xml.contains("R&amp;D\\O&apos;Brien"));
    }

    #[test]
    fn generated_xml_overrides_the_battery_defaults() {
        // Both default to true, which would make the widget do nothing on an unplugged laptop.
        let xml = build_xml(&ALL_TASKS[1], r"C:\x\linkswitch.exe", "PC\\user");
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
    }

    #[test]
    fn generated_xml_requests_elevation_and_on_demand_start() {
        let xml = build_xml(&ALL_TASKS[1], r"C:\x\linkswitch.exe", "PC\\user");
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<AllowStartOnDemand>true</AllowStartOnDemand>"));
        // IgnoreNew would silently drop a second click.
        assert!(xml.contains("<MultipleInstancesPolicy>Queue</MultipleInstancesPolicy>"));
    }

    #[test]
    fn only_the_recovery_task_has_a_trigger() {
        for spec in ALL_TASKS {
            let xml = build_xml(spec, r"C:\x\linkswitch.exe", "PC\\user");
            assert_eq!(
                xml.contains("<LogonTrigger>"),
                spec.logon_trigger,
                "{} trigger mismatch",
                spec.name
            );
        }
    }

    #[test]
    fn every_action_is_our_own_exe_never_a_shell() {
        for spec in ALL_TASKS {
            let xml = build_xml(spec, r"C:\Program Files\LinkSwitch\linkswitch.exe", "PC\\u");
            assert!(xml.contains(r"<Command>C:\Program Files\LinkSwitch\linkswitch.exe</Command>"));
            let lower = spec.args.to_ascii_lowercase();
            assert!(!lower.contains("cmd") && !lower.contains("powershell"));
        }
    }

    #[test]
    fn task_arguments_are_fixed_verbs_with_no_substitution() {
        // A task that runs whatever it is told with an elevated token would be an integrity
        // bridge for any process running as this user.
        for spec in ALL_TASKS {
            assert!(
                !spec.args.contains("$(Arg"),
                "{} must not take runtime arguments",
                spec.name
            );
        }
    }

    #[test]
    fn command_round_trips_out_of_the_generated_xml() {
        let exe = r"C:\Program Files\Link&Switch\linkswitch.exe";
        let xml = build_xml(&ALL_TASKS[0], exe, "PC\\user");
        assert_eq!(extract_command(&xml).as_deref(), Some(exe));
    }

    #[test]
    fn unescape_does_not_double_expand() {
        // "&amp;lt;" is a literal "&lt;", not a "<".
        assert_eq!(unescape("&amp;lt;"), "&lt;");
        assert_eq!(unescape("a&amp;b"), "a&b");
    }

    #[test]
    fn every_mode_maps_to_a_registered_task() {
        for mode in [Mode::Ethernet, Mode::Wifi, Mode::Auto] {
            assert!(
                ALL_TASKS.iter().any(|t| t.name == mode.task_name()),
                "{} has no task",
                mode.as_str()
            );
        }
    }

    #[test]
    fn task_paths_are_rooted_in_our_folder() {
        assert_eq!(task_path("ApplyWifi"), r"\LinkSwitch\ApplyWifi");
    }
}
