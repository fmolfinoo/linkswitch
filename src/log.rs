//! File logging.
//!
//! The elevated worker runs under Task Scheduler with no console and no window, so a log file is
//! its *only* diagnostics channel. When something goes wrong on a user's machine, this file is
//! the entire bug report.
//!
//! Two log files, because the two roles run at different privilege levels and the machine
//! directory is deliberately not writable by the unelevated widget:
//!   - worker: `%ProgramData%\LinkSwitch\logs\worker.log`
//!   - widget: `%LOCALAPPDATA%\LinkSwitch\widget.log`

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static SINK: Mutex<Option<PathBuf>> = Mutex::new(None);

const MAX_BYTES: u64 = 1_000_000;

/// Point the log at a file and record which role is running.
pub fn init(path: PathBuf, role: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    rotate_if_large(&path);
    *SINK.lock().unwrap() = Some(path);
    line(&format!(
        "--- linkswitch {} start (role={role}, pid={}) ---",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
}

fn rotate_if_large(path: &std::path::Path) {
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > MAX_BYTES {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
    }
}

/// Append one line. Never panics and never fails the caller: losing a log line must not take
/// down a network operation.
pub fn line(msg: &str) {
    let stamp = timestamp();
    let guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(path) = guard.as_ref() {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{stamp} {msg}");
        }
    }
    // Also to stdout when a console is attached, so the CLI verbs are usable interactively.
    println!("{msg}");
}

/// Seconds since the Unix epoch, formatted as a UTC calendar time.
///
/// Hand-rolled rather than pulling in a date crate: this is the only place LinkSwitch needs a
/// formatted time, and a dependency in an elevated binary should have to earn its place.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs as i64);
    let mut out = String::new();
    let _ = write!(out, "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    out
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

#[macro_export]
macro_rules! lslog {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::civil_from_unix;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn known_timestamps_decode() {
        // 2026-09-07T00:00:00Z
        assert_eq!(civil_from_unix(1_788_739_200), (2026, 9, 7, 0, 0, 0));
        // A leap day, which is where naive date maths goes wrong.
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn time_of_day_is_decoded() {
        assert_eq!(civil_from_unix(86_399), (1970, 1, 1, 23, 59, 59));
        assert_eq!(civil_from_unix(86_400), (1970, 1, 2, 0, 0, 0));
    }
}
