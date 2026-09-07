// A GUI subsystem binary shows no console window when double-clicked -- which is what a desktop
// widget must do. The CLI verbs then have no stdout, so `attach_console_if_cli` borrows the
// parent's console when there is one. Debug builds keep the console subsystem so `cargo run`
// behaves normally.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apply;
mod cli;
mod config;
mod elevate;
mod install;
mod log;
mod net;
mod tasks;
mod ui;

use std::process::ExitCode;

use cli::Cmd;

fn main() -> ExitCode {
    attach_console_if_cli();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("linkswitch: {}", e.0);
            eprintln!("Try `linkswitch --help`.");
            return ExitCode::from(apply::exit::GENERIC as u8);
        }
    };

    match cmd {
        Cmd::Gui => {
            log::init(config::widget_log_path(), "widget");
            match ui::run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("linkswitch: {e}");
                    ExitCode::FAILURE
                }
            }
        }

        Cmd::Apply(mode) => {
            log::init(config::worker_log_path(), "worker");
            install_panic_hook();
            let report = apply::apply(mode);
            println!("{}", report.message);
            for d in &report.details {
                println!("  {d}");
            }
            ExitCode::from(report.exit_code as u8)
        }

        Cmd::Recover => {
            log::init(config::worker_log_path(), "recover");
            install_panic_hook();
            if !elevate::is_elevated() {
                eprintln!("linkswitch: --recover needs administrator rights.");
                return ExitCode::from(apply::exit::NOT_ELEVATED as u8);
            }
            apply::recover_if_torn();
            ExitCode::SUCCESS
        }

        Cmd::Install {
            keep_wifi,
            autostart,
        } => {
            if !elevate::is_elevated() {
                // One prompt, here, is the entire UAC budget for this application.
                let mut argv = vec!["--install"];
                if keep_wifi {
                    argv.push("--keep-wifi-connected");
                }
                if !autostart {
                    argv.push("--no-autostart");
                }
                return match install::self_elevate(&argv) {
                    Ok(code) => ExitCode::from(code as u8),
                    Err(e) => {
                        eprintln!("linkswitch: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            log::init(config::worker_log_path(), "install");
            let outcome = install::install(keep_wifi, autostart);
            for m in &outcome.messages {
                println!("{m}");
            }
            if outcome.ok {
                println!("\nLinkSwitch is ready. Run it with no arguments to open the widget.");
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }

        Cmd::Uninstall => {
            if !elevate::is_elevated() {
                return match install::self_elevate(&["--uninstall"]) {
                    Ok(code) => ExitCode::from(code as u8),
                    Err(e) => {
                        eprintln!("linkswitch: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            log::init(config::worker_log_path(), "uninstall");
            let outcome = install::uninstall();
            for m in &outcome.messages {
                println!("{m}");
            }
            if outcome.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }

        Cmd::Status { json } => {
            print_status(json);
            ExitCode::SUCCESS
        }

        Cmd::Help => {
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }

        Cmd::Version => {
            println!("linkswitch {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}

/// Borrow the parent console so CLI output is visible from a terminal.
///
/// `AttachConsole` fails when the parent has no console, which is exactly the scheduled-task
/// case -- and is why the worker's real diagnostics channel is the log file. Handles are already
/// valid when output is redirected, so `linkswitch --status > out.txt` works either way.
fn attach_console_if_cli() {
    if std::env::args().len() <= 1 {
        return;
    }
    #[cfg(not(debug_assertions))]
    // SAFETY: attaching to the parent's console has no preconditions, and failure is expected
    // and ignored when there is no console to attach to.
    unsafe {
        use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Make a panic in the elevated worker visible.
///
/// Without this the worker dies silently and the log file -- its only diagnostics channel -- is
/// empty precisely when something has gone wrong.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::line(&format!("PANIC: {info}"));
        previous(info);
    }));
}

fn print_status(json: bool) {
    let snap = net::Snapshot::read();
    let cfg = config::load_machine();
    let journal = config::load_journal();
    let (eth_c, wifi_c) = snap.candidates();

    let eth = config::resolve(cfg.ethernet.as_ref(), net::adapters::NicKind::Ethernet, &snap.nics).or_else(|| eth_c.first().copied());
    let wifi = config::resolve(cfg.wifi.as_ref(), net::adapters::NicKind::Wifi, &snap.nics).or_else(|| wifi_c.first().copied());
    let verdict = snap.verdict(eth.map(|n| n.luid), wifi.map(|n| n.luid));
    let wifi_status = net::wifi::status();
    let policy = net::wcm::effective();
    let health = tasks::health().unwrap_or_default();

    if json {
        // Hand-built rather than derived across the whole tree: this is a stable diagnostic
        // contract for bug reports and should not change silently whenever an internal struct
        // gains a field.
        let esc = |s: &str| s.replace('\\', r"\\").replace('"', "\\\"");
        let nic_json = |n: Option<&net::adapters::Nic>| match n {
            Some(n) => format!(
                r#"{{"name":"{}","if_index":{},"luid":"{}","permanent_mac":{},"up":{},"media_connected":{},"ipv4":{:?}}}"#,
                esc(n.label()),
                n.if_index,
                n.luid,
                n.permanent_mac
                    .map(|m| format!("\"{}\"", config::format_mac(m)))
                    .unwrap_or_else(|| "null".into()),
                n.oper_up,
                n.media_connected,
                n.ipv4.iter().map(|a| a.to_string()).collect::<Vec<_>>()
            ),
            None => "null".into(),
        };
        println!("{{");
        println!(r#"  "version": "{}","#, env!("CARGO_PKG_VERSION"));
        println!(r#"  "installed": {},"#, install::is_installed());
        println!(r#"  "elevated": {},"#, elevate::is_elevated());
        println!(r#"  "ethernet": {},"#, nic_json(eth));
        println!(r#"  "wifi": {},"#, nic_json(wifi));
        println!(r#"  "verdict": "{verdict:?}","#);
        println!(
            r#"  "wifi_ssid": {},"#,
            wifi_status
                .as_ref()
                .and_then(|w| w.ssid.as_deref())
                .map(|s| format!("\"{}\"", esc(s)))
                .unwrap_or_else(|| "null".into())
        );
        println!(r#"  "wcm_policy": "{:?}","#, policy.policy);
        println!(r#"  "wcm_is_group_policy": {},"#, policy.is_group_policy);
        println!(r#"  "journal_mode": "{}","#, journal.mode.as_str());
        println!(r#"  "journal_torn": {},"#, journal.is_torn());
        println!(
            r#"  "tasks": [{}]"#,
            health
                .iter()
                .map(|t| format!(
                    r#"{{"name":"{}","registered":{},"last_result":{}}}"#,
                    t.name,
                    t.registered,
                    t.last_result
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "null".into())
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        println!("}}");
        return;
    }

    println!("LinkSwitch {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  installed:        {}",
        if install::is_installed() {
            "yes"
        } else {
            "no  (run `linkswitch --install`)"
        }
    );

    let show = |label: &str, n: Option<&net::adapters::Nic>| match n {
        Some(n) => {
            let state = if !n.media_connected {
                if n.kind == net::adapters::NicKind::Ethernet {
                    "cable unplugged"
                } else {
                    "not connected"
                }
                .to_string()
            } else {
                n.ipv4
                    .first()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "no address".into())
            };
            let m = net::metric::read(n.luid, windows::Win32::Networking::WinSock::AF_INET)
                .map(|s| {
                    if s.automatic {
                        format!("metric {} (automatic)", s.metric)
                    } else {
                        format!("metric {} (pinned)", s.metric)
                    }
                })
                .unwrap_or_else(|_| "no IPv4".into());
            println!("  {label:<17} {} - {state}, {m}", n.label());
        }
        None => println!("  {label:<17} not found"),
    };
    show("ethernet:", eth);
    show("wi-fi:", wifi);

    if let Some(w) = &wifi_status {
        if w.connected {
            println!(
                "                    connected to \"{}\" ({}%)",
                w.ssid.as_deref().unwrap_or("?"),
                w.signal_quality.unwrap_or(0)
            );
        } else if !w.radio_on {
            println!("                    radio is off");
        }
    }

    for (label, n) in [("ethernet", eth), ("wi-fi", wifi)] {
        let Some(n) = n else { continue };
        match net::binding::read(&n.adapter_name) {
            Ok(b) => {
                println!(
                    "  {:<17} IPv4 {}  IPv6 {}",
                    format!("{label} ip stack:"),
                    if b.v4 { "bound" } else { "UNBOUND" },
                    if b.v6 { "bound" } else { "unbound" }
                );
                if let Some(w) = net::binding::bridge_warning(&n.adapter_name) {
                    println!("                    note: {w}");
                }
            }
            Err(e) => println!(
                "  {:<17} unreadable - {}",
                format!("{label} ip stack:"),
                e.user_message()
            ),
        }
    }
    println!("  carrying traffic: {}", describe_verdict(&verdict, &snap));
    println!("  windows policy:   {}", policy.policy.describe());
    if policy.is_group_policy {
        println!("                    (set by Group Policy)");
    }
    if journal.is_torn() {
        println!("  WARNING: a previous switch was interrupted; run `linkswitch --recover`.");
    }
}

fn describe_verdict(v: &net::routes::Verdict, snap: &net::Snapshot) -> String {
    use net::routes::Verdict;
    match v {
        Verdict::Ethernet { total } => format!("Ethernet (total metric {total})"),
        Verdict::Wifi { total } => format!("Wi-Fi (total metric {total})"),
        Verdict::Hijacked {
            luid,
            if_index,
            total,
        } => {
            let name = snap
                .nic(*luid)
                .map(|n| n.label().to_string())
                .unwrap_or_else(|| format!("interface {if_index}"));
            format!("{name} - a VPN or tunnel holds the default route (total metric {total})")
        }
        Verdict::None => "nothing - no default route".into(),
    }
}
