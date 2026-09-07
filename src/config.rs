//! Persisted state: what to steer, what we changed, and how to put it back.
//!
//! Three stores, split by who is allowed to write them:
//!
//! | Store | Path | Writer |
//! |---|---|---|
//! | [`MachineConfig`] | `%ProgramData%\LinkSwitch\config.json` | elevated only |
//! | [`Journal`] | `%ProgramData%\LinkSwitch\state.json` | elevated worker only |
//! | [`UserPrefs`] | `%LOCALAPPDATA%\LinkSwitch\prefs.json` | the widget |
//!
//! The split matters for security: the unelevated widget must not be able to write anything the
//! elevated worker later reads and acts on, or a medium-integrity process could steer the
//! worker's behaviour. The widget only ever asks the worker to run one of a few fixed verbs.
//!
//! # The restore rule
//!
//! LinkSwitch never restores a value it did not capture, and never writes a hardcoded "default"
//! in place of one. Everything it changes is journalled with its **prior** value first, and
//! restore writes that exact value back.
//!
//! This is not fussiness. On the development machine a VPN has applied IPv6 leak protection by
//! unbinding `ms_tcpip6` from every adapter. A restore routine that "helpfully" put things back
//! to Windows defaults would silently switch IPv6 back on and leak the user's real address --
//! damage far worse than the problem it was fixing. Same for metrics: a user who had manually
//! pinned Ethernet to 10 before installing gets 10 back, not "automatic".

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::net::LuidKey;

/// Bumped whenever a stored struct changes shape. Loading tolerates older versions and anything
/// unparsable falls back to defaults rather than bricking the app.
pub const SCHEMA: u32 = 1;

/// Which link should carry internet traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Ethernet wins; Wi-Fi is the standby.
    Ethernet,
    /// Wi-Fi wins. Ethernet stays enabled, link-up and reachable on its own subnet, so a NAS or
    /// printer on the wire keeps working.
    Wifi,
    /// Hand both interfaces back to Windows' automatic metrics.
    Auto,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ethernet => "ethernet",
            Mode::Wifi => "wifi",
            Mode::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ethernet" | "eth" | "wired" => Some(Mode::Ethernet),
            "wifi" | "wi-fi" | "wireless" => Some(Mode::Wifi),
            "auto" | "automatic" | "restore" => Some(Mode::Auto),
            _ => None,
        }
    }

    /// The scheduled-task name that applies this mode.
    pub fn task_name(self) -> &'static str {
        match self {
            Mode::Ethernet => "ApplyEthernet",
            Mode::Wifi => "ApplyWifi",
            Mode::Auto => "ApplyAuto",
        }
    }
}

/// A durable reference to an adapter.
///
/// The LUID is the primary key, but it is not eternal: reinstalling a driver or swapping a
/// docking station mints a new one. The descriptive fields let [`resolve`] recover silently
/// instead of telling the user their adapter vanished.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterRef {
    pub luid: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Permanent MAC, lower-case hex, colon-separated. Survives driver reinstalls.
    #[serde(default)]
    pub mac: Option<String>,
}

impl AdapterRef {
    pub fn key(&self) -> LuidKey {
        LuidKey(self.luid)
    }
}

/// Prior state of the WCM "minimize simultaneous connections" policy value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WcmBackup {
    /// Whether the registry value existed at all before we touched it. Restoring must *delete*
    /// the value when it was originally absent -- writing a 0 back would leave the policy
    /// explicitly disabled rather than at the Windows default.
    pub present: bool,
    pub value: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineConfig {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub ethernet: Option<AdapterRef>,
    #[serde(default)]
    pub wifi: Option<AdapterRef>,
    /// Metric written to the losing interface. Validated against the allowlist in
    /// [`crate::net::metric`] regardless of what the file says.
    #[serde(default = "default_park")]
    pub park_metric: u32,
    #[serde(default)]
    pub wcm_backup: Option<WcmBackup>,
    /// Last Wi-Fi profile seen connected, used as the fallback target when the worker has to
    /// associate Wi-Fi itself.
    #[serde(default)]
    pub wifi_profile_hint: Option<String>,
}

fn default_schema() -> u32 {
    SCHEMA
}
fn default_park() -> u32 {
    crate::net::metric::PARK_DEFAULT
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            ethernet: None,
            wifi: None,
            park_metric: crate::net::metric::PARK_DEFAULT,
            wcm_backup: None,
            wifi_profile_hint: None,
        }
    }
}

/// One interface/family metric as it was before we touched it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricBackup {
    pub luid: u64,
    pub family_v6: bool,
    pub metric: u32,
    pub automatic: bool,
}

/// The crash-recovery journal.
///
/// Written **before** any change is made and cleared only after the change is confirmed. If the
/// worker is killed mid-apply -- and it can be, since the task allows hard termination -- this
/// file is what a later run uses to notice a torn state and put the machine back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    #[serde(default = "default_schema")]
    pub schema: u32,
    /// The mode being applied, or the last one successfully applied.
    pub mode: Mode,
    /// True between "about to change something" and "finished". A journal found with this still
    /// set means the previous run died partway through.
    #[serde(default)]
    pub in_flight: bool,
    #[serde(default)]
    pub started_unix: u64,
    #[serde(default)]
    pub finished_unix: Option<u64>,
    /// Prior values for everything touched, so restore is exact rather than "set to default".
    #[serde(default)]
    pub restore: Vec<MetricBackup>,
    #[serde(default)]
    pub last_error: Option<String>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            mode: Mode::Auto,
            in_flight: false,
            started_unix: 0,
            finished_unix: None,
            restore: Vec::new(),
            last_error: None,
        }
    }
}

impl Journal {
    /// A previous run died between changing something and confirming it.
    pub fn is_torn(&self) -> bool {
        self.in_flight && !self.restore.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPrefs {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub pos: Option<(f32, f32)>,
    #[serde(default)]
    pub start_hidden: bool,
}

impl Default for UserPrefs {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            pos: None,
            start_hidden: false,
        }
    }
}

// --- paths -------------------------------------------------------------------------------

/// `%ProgramData%\LinkSwitch` -- machine-wide, admin-writable only.
pub fn machine_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("LinkSwitch")
}

/// `%LOCALAPPDATA%\LinkSwitch` -- per-user, writable by the unelevated widget.
pub fn user_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("LinkSwitch")
}

pub fn config_path() -> PathBuf {
    machine_dir().join("config.json")
}
pub fn journal_path() -> PathBuf {
    machine_dir().join("state.json")
}
pub fn prefs_path() -> PathBuf {
    user_dir().join("prefs.json")
}
pub fn worker_log_path() -> PathBuf {
    machine_dir().join("logs").join("worker.log")
}
pub fn widget_log_path() -> PathBuf {
    user_dir().join("widget.log")
}

// --- load / save -------------------------------------------------------------------------

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &std::path::Path) -> T {
    // A missing or corrupt file must degrade to defaults, never abort. This runs in an elevated
    // process during a network change; refusing to start because a JSON file has a stray byte
    // would be the worst possible failure mode.
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

/// Write atomically: a crash mid-write must not leave a truncated file behind.
fn save_json<T: Serialize>(path: &std::path::Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, body)?;
    // Windows rename fails if the destination exists, so replace explicitly.
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path)
        }
    }
}

pub fn load_machine() -> MachineConfig {
    load_json(&config_path())
}
pub fn save_machine(c: &MachineConfig) -> std::io::Result<()> {
    save_json(&config_path(), c)
}
pub fn load_journal() -> Journal {
    load_json(&journal_path())
}
pub fn save_journal(j: &Journal) -> std::io::Result<()> {
    save_json(&journal_path(), j)
}
pub fn load_prefs() -> UserPrefs {
    load_json(&prefs_path())
}
pub fn save_prefs(p: &UserPrefs) -> std::io::Result<()> {
    save_json(&prefs_path(), p)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Re-resolve a saved adapter against what is present now.
///
/// Tries the LUID, then the permanent MAC, then the friendly name, then the description. A dock
/// being plugged in or a driver being reinstalled changes the LUID, and that is a daily event
/// rather than an exotic edge case, so a LUID-only lookup would tell the user their adapter had
/// disappeared on a regular basis.
pub fn resolve<'a>(
    saved: Option<&AdapterRef>,
    nics: &'a [crate::net::adapters::Nic],
) -> Option<&'a crate::net::adapters::Nic> {
    let saved = saved?;
    if saved.luid != 0 {
        if let Some(n) = nics.iter().find(|n| n.luid.0 == saved.luid) {
            return Some(n);
        }
    }
    if let Some(mac) = saved.mac.as_deref() {
        if let Some(n) = nics
            .iter()
            .find(|n| n.mac.map(format_mac).as_deref() == Some(mac))
        {
            return Some(n);
        }
    }
    if !saved.name.is_empty() {
        if let Some(n) = nics.iter().find(|n| n.friendly_name == saved.name) {
            return Some(n);
        }
    }
    if !saved.description.is_empty() {
        if let Some(n) = nics.iter().find(|n| n.description == saved.description) {
            return Some(n);
        }
    }
    None
}

pub fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

impl From<&crate::net::adapters::Nic> for AdapterRef {
    fn from(n: &crate::net::adapters::Nic) -> Self {
        Self {
            luid: n.luid.0,
            name: n.friendly_name.clone(),
            description: n.description.clone(),
            mac: n.mac.map(format_mac),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::adapters::{Nic, NicKind, Tier};

    fn nic(luid: u64, name: &str, desc: &str, mac: Option<[u8; 6]>) -> Nic {
        Nic {
            luid: LuidKey(luid),
            if_index: 1,
            friendly_name: name.into(),
            description: desc.into(),
            adapter_name: "{guid}".into(),
            if_type: 6,
            kind: NicKind::Ethernet,
            tier: Tier::Hardware,
            oper_up: true,
            media_connected: true,
            tx_speed_bps: Some(1_000_000_000),
            mac,
            ipv4: vec![],
            ipv6: vec![],
            gateways: vec![],
        }
    }

    #[test]
    fn mode_round_trips_through_strings() {
        for m in [Mode::Ethernet, Mode::Wifi, Mode::Auto] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        assert_eq!(Mode::parse("  WiFi "), Some(Mode::Wifi));
        assert_eq!(Mode::parse("wired"), Some(Mode::Ethernet));
        assert_eq!(Mode::parse("nonsense"), None);
    }

    #[test]
    fn resolve_prefers_the_luid() {
        let nics = [nic(1, "Ethernet", "Intel I225-V", None), nic(2, "Other", "X", None)];
        let saved = AdapterRef {
            luid: 2,
            name: "Ethernet".into(),
            description: "Intel I225-V".into(),
            mac: None,
        };
        // LUID wins even though the name matches the other adapter.
        assert_eq!(resolve(Some(&saved), &nics).unwrap().luid.0, 2);
    }

    #[test]
    fn resolve_falls_back_to_mac_when_the_luid_changed() {
        // A dock reconnect or driver reinstall mints a new LUID; the MAC is stable.
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x02];
        let nics = [nic(999, "Ethernet 2", "Intel I225-V", Some(mac))];
        let saved = AdapterRef {
            luid: 1,
            name: "Ethernet".into(),
            description: "Intel I225-V".into(),
            mac: Some(format_mac(mac)),
        };
        assert_eq!(resolve(Some(&saved), &nics).unwrap().luid.0, 999);
    }

    #[test]
    fn resolve_falls_back_to_name_then_description() {
        let nics = [nic(999, "Ethernet", "Intel I225-V", None)];
        let by_name = AdapterRef {
            luid: 1,
            name: "Ethernet".into(),
            description: "something else".into(),
            mac: None,
        };
        assert_eq!(resolve(Some(&by_name), &nics).unwrap().luid.0, 999);

        let by_desc = AdapterRef {
            luid: 1,
            name: "renamed by the user".into(),
            description: "Intel I225-V".into(),
            mac: None,
        };
        assert_eq!(resolve(Some(&by_desc), &nics).unwrap().luid.0, 999);
    }

    #[test]
    fn resolve_gives_up_when_nothing_matches() {
        let nics = [nic(999, "Wi-Fi", "RZ608", None)];
        let saved = AdapterRef {
            luid: 1,
            name: "Ethernet".into(),
            description: "Intel I225-V".into(),
            mac: None,
        };
        assert!(resolve(Some(&saved), &nics).is_none());
        assert!(resolve(None, &nics).is_none());
    }

    #[test]
    fn a_journal_left_in_flight_is_torn() {
        let mut j = Journal::default();
        assert!(!j.is_torn());
        j.in_flight = true;
        // In-flight with nothing recorded yet means nothing was changed.
        assert!(!j.is_torn());
        j.restore.push(MetricBackup {
            luid: 1,
            family_v6: false,
            metric: 5,
            automatic: true,
        });
        assert!(j.is_torn());
        j.in_flight = false;
        assert!(!j.is_torn());
    }

    #[test]
    fn unknown_fields_and_missing_fields_survive_a_round_trip() {
        // Forward compatibility: a config written by a later version must not brick this one.
        let json = r#"{"schema":99,"park_metric":1234,"future_field":true}"#;
        let c: MachineConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.park_metric, 1234);
        assert!(c.ethernet.is_none());

        // And a totally empty object gets sane defaults.
        let c: MachineConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c.park_metric, crate::net::metric::PARK_DEFAULT);
    }

    #[test]
    fn corrupt_config_degrades_to_defaults_rather_than_failing() {
        let c: MachineConfig = serde_json::from_str("not json at all").unwrap_or_default();
        assert_eq!(c.schema, SCHEMA);
    }

    #[test]
    fn mac_formatting_is_stable_lowercase_hex() {
        assert_eq!(
            format_mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x02]),
            "aa:bb:cc:dd:ee:02"
        );
        assert_eq!(format_mac([0, 0, 0, 0, 0, 0]), "00:00:00:00:00:00");
    }
}
