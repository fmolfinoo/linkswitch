//! Argument parsing.
//!
//! Hand-rolled rather than pulling in a parser crate: there are seven verbs, and a dependency
//! that ends up inside an elevated worker should have to earn its place. Anything unrecognised
//! is rejected rather than ignored -- an elevated process that quietly does something other than
//! what it was asked is not acceptable.

use crate::config::Mode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// No arguments: run the desktop widget.
    Gui,
    /// Elevated worker. Started by a scheduled task.
    Apply(Mode),
    /// Elevated. Undo a half-finished change left by an interrupted switch. Runs at logon.
    Recover,
    /// Elevated. Register tasks and set the app up.
    Install {
        /// Also let Windows keep Wi-Fi connected while Ethernet is plugged in.
        keep_wifi: bool,
        /// Start the widget at logon.
        autostart: bool,
    },
    /// Elevated. Restore every metric, remove tasks and files.
    Uninstall,
    /// Print what LinkSwitch sees. Unprivileged.
    Status { json: bool },
    Help,
    Version,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

pub fn parse<I, S>(args: I) -> Result<Cmd, ParseError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let mut it = args.iter().peekable();

    let Some(first) = it.next() else {
        return Ok(Cmd::Gui);
    };

    match first.as_str() {
        "--apply" => {
            let Some(m) = it.next() else {
                return Err(ParseError(
                    "--apply needs a mode: ethernet, wifi, wifi-only or auto".into(),
                ));
            };
            let Some(mode) = Mode::parse(m) else {
                return Err(ParseError(format!(
                    "unknown mode \"{m}\"; expected ethernet, wifi, wifi-only or auto"
                )));
            };
            reject_extra(it, Cmd::Apply(mode))
        }
        "--recover" => reject_extra(it, Cmd::Recover),
        "--install" => {
            let mut keep_wifi = false;
            let mut autostart = true;
            for a in it {
                match a.as_str() {
                    "--keep-wifi-connected" => keep_wifi = true,
                    "--no-autostart" => autostart = false,
                    other => return Err(ParseError(format!("unknown option \"{other}\""))),
                }
            }
            Ok(Cmd::Install {
                keep_wifi,
                autostart,
            })
        }
        "--uninstall" => reject_extra(it, Cmd::Uninstall),
        "--status" => {
            let mut json = false;
            for a in it {
                match a.as_str() {
                    "--json" => json = true,
                    other => return Err(ParseError(format!("unknown option \"{other}\""))),
                }
            }
            Ok(Cmd::Status { json })
        }
        "--help" | "-h" | "/?" => Ok(Cmd::Help),
        "--version" | "-V" => Ok(Cmd::Version),
        other => Err(ParseError(format!(
            "unknown command \"{other}\"; try --help"
        ))),
    }
}

fn reject_extra<'a, I: Iterator<Item = &'a String>>(
    mut it: I,
    cmd: Cmd,
) -> Result<Cmd, ParseError> {
    match it.next() {
        None => Ok(cmd),
        Some(x) => Err(ParseError(format!("unexpected argument \"{x}\""))),
    }
}

pub const HELP: &str = r#"LinkSwitch - choose whether Windows sends your traffic over Ethernet or Wi-Fi,
without unplugging the cable.

USAGE:
  linkswitch                     Run the desktop widget.
  linkswitch --status [--json]   Show what LinkSwitch sees. Needs no admin rights.
  linkswitch --install [OPTIONS] Set up: register the scheduled tasks that let the widget
                                 switch without a UAC prompt every time. Asks for admin once.
  linkswitch --uninstall         Restore every metric LinkSwitch changed and remove everything.
  linkswitch --apply MODE        Apply a mode directly. MODE is ethernet, wifi, wifi-only
                                 or auto.
                                 Needs admin; normally started by a scheduled task.
  linkswitch --recover           Undo a half-finished change left by an interrupted switch.
  linkswitch --help / --version

INSTALL OPTIONS:
  --keep-wifi-connected   Also turn off Windows' "minimize simultaneous connections" policy,
                          so Wi-Fi stays associated while Ethernet is plugged in. Off by
                          default: it is a machine-wide policy and is compliance-relevant on
                          managed systems. LinkSwitch works without it by connecting Wi-Fi
                          on demand.
  --no-autostart          Do not start the widget at logon.

MODES:
  ethernet   Ethernet carries your traffic.
  wifi       Wi-Fi carries your traffic. Ethernet stays connected and keeps serving its own
             subnet, so a NAS or printer on the wire still works.
  wifi-only  As above, but Ethernet's IP stack is detached entirely: no address, no routes,
             no DNS. The cable stays plugged in and the adapter stays enabled. This is zero
             IP, not zero traffic -- LLDP, network discovery and any VM bridge still reach
             the wire.
  auto       Hand both adapters back to Windows' automatic metrics.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_runs_the_widget() {
        assert_eq!(parse(Vec::<String>::new()), Ok(Cmd::Gui));
    }

    #[test]
    fn apply_accepts_every_mode() {
        assert_eq!(parse(["--apply", "wifi"]), Ok(Cmd::Apply(Mode::Wifi)));
        assert_eq!(
            parse(["--apply", "wifi-only"]),
            Ok(Cmd::Apply(Mode::WifiOnly))
        );
        assert_eq!(
            parse(["--apply", "ethernet"]),
            Ok(Cmd::Apply(Mode::Ethernet))
        );
        assert_eq!(parse(["--apply", "auto"]), Ok(Cmd::Apply(Mode::Auto)));
    }

    #[test]
    fn apply_rejects_a_missing_or_bogus_mode() {
        assert!(parse(["--apply"]).is_err());
        assert!(parse(["--apply", "sideways"]).is_err());
    }

    #[test]
    fn unknown_commands_and_stray_arguments_are_rejected() {
        // An elevated process must never quietly do something other than what it was asked.
        assert!(parse(["--destroy"]).is_err());
        assert!(parse(["--apply", "wifi", "extra"]).is_err());
        assert!(parse(["--uninstall", "now"]).is_err());
        assert!(parse(["--status", "--verbose"]).is_err());
    }

    #[test]
    fn install_defaults_to_autostart_and_leaves_the_policy_alone() {
        // The WCM policy is machine-wide and compliance-relevant, so it must be opt-in.
        assert_eq!(
            parse(["--install"]),
            Ok(Cmd::Install {
                keep_wifi: false,
                autostart: true
            })
        );
        assert_eq!(
            parse(["--install", "--keep-wifi-connected", "--no-autostart"]),
            Ok(Cmd::Install {
                keep_wifi: true,
                autostart: false
            })
        );
    }

    #[test]
    fn status_takes_an_optional_json_flag() {
        assert_eq!(parse(["--status"]), Ok(Cmd::Status { json: false }));
        assert_eq!(parse(["--status", "--json"]), Ok(Cmd::Status { json: true }));
    }

    #[test]
    fn help_and_version_have_the_usual_spellings() {
        for a in ["--help", "-h", "/?"] {
            assert_eq!(parse([a]), Ok(Cmd::Help));
        }
        for a in ["--version", "-V"] {
            assert_eq!(parse([a]), Ok(Cmd::Version));
        }
    }

    #[test]
    fn help_text_documents_every_verb() {
        for verb in [
            "--status",
            "--install",
            "--uninstall",
            "--apply",
            "--recover",
        ] {
            assert!(HELP.contains(verb), "{verb} is missing from --help");
        }
    }
}
