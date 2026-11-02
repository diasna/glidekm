//! Minimal logger: stderr and/or an optional file. No dependencies.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static SINK: Mutex<Option<File>> = Mutex::new(None);
static VERBOSE: AtomicBool = AtomicBool::new(false);

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
