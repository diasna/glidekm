//! Minimal logger: stderr and/or an optional file. No dependencies.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static SINK: Mutex<Option<File>> = Mutex::new(None);
static VERBOSE: AtomicBool = AtomicBool::new(false);
/// Recent lines kept in memory for UIs: (sequence number, line).
static RECENT: Mutex<(u64, VecDeque<(u64, String)>)> = Mutex::new((0, VecDeque::new()));
const RECENT_MAX: usize = 300;

/// Log lines with sequence number >= `since`, and the next sequence number.
pub fn recent(since: u64) -> (Vec<(u64, String)>, u64) {
    let g = RECENT.lock().unwrap();
    let lines = g.1.iter().filter(|(n, _)| *n >= since).cloned().collect();
    (lines, g.0)
}

pub fn set_file(f: File) {
    *SINK.lock().unwrap() = Some(f);
}

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed)
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn write(level: &str, msg: std::fmt::Arguments) {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = t.as_secs() % 86400;
    let line = format!(
        "{:02}:{:02}:{:02}.{:03} {level} {msg}\n",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        t.subsec_millis()
    );
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Ok(mut g) = RECENT.lock() {
        let n = g.0;
        g.0 += 1;
        g.1.push_back((n, line.trim_end().to_string()));
        if g.1.len() > RECENT_MAX {
            g.1.pop_front();
        }
    }
    if let Ok(mut g) = SINK.lock() {
        if let Some(f) = g.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[macro_export]
macro_rules! info { ($($a:tt)*) => { $crate::log::write("INFO ", format_args!($($a)*)) } }
#[macro_export]
macro_rules! warn { ($($a:tt)*) => { $crate::log::write("WARN ", format_args!($($a)*)) } }
#[macro_export]
macro_rules! debug { ($($a:tt)*) => { if $crate::log::verbose() { $crate::log::write("DEBUG", format_args!($($a)*)) } } }
