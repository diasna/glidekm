//! glidekm-server: shares this Windows PC's mouse and keyboard with a SteamOS
//! client that behaves like an extra monitor.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod control;
mod engine;
mod hotkey;
mod layout;
mod net;
#[cfg(windows)]
mod scan;
#[cfg(windows)]
mod settings;
#[cfg(not(windows))]
mod sim;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod winupdate;

use control::{Control, ServerConfig};
use glidekm_core::config::Config;
use glidekm_core::event::Event;
use glidekm_core::{info, warn};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub const USAGE: &str = "glidekm-server [--config FILE] [--client HOST[:PORT]] [--passphrase P]
                [--side right|left|top|bottom] [--offset PX] [--scale F]
                [--hotkey-switch ctrl+alt+shift+s] [--hotkey-capture ctrl+alt+shift+m]
                [--open-ui] [--console] [--log-file FILE] [--verbose]
Settings are normally edited in the settings window (tray icon -> Open settings).
Config file default: glidekm-server.conf next to the executable.";

/// Command-line / runtime options that are not part of the saved config.
pub struct Opts {
    pub flags: Config,
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Start networking. Returns the input channel.
pub fn start_services(ctl: &Arc<Control>) -> Result<Sender<Event>, String> {
    net::spawn(ctl.shared.clone()).map_err(|e| format!("network: {e}"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        #[cfg(windows)]
        win::attach_console();
        println!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        #[cfg(windows)]
        win::attach_console();
        println!("glidekm-server {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut flags = Config::default();
    flags.apply_args(&args);
    #[cfg(windows)]
    if flags.flag("console") {
        win::attach_console();
    }
    glidekm_core::log::set_verbose(flags.flag("verbose"));

    // GUI app: default to a log file next to the exe (kept small).
    let log_path = flags
        .get("log_file")
        .map(PathBuf::from)
        .or_else(|| cfg!(windows).then(|| exe_dir().join("glidekm-server.log")));
    if let Some(p) = &log_path {
        let big = std::fs::metadata(p)
            .map(|m| m.len() > 1 << 20)
            .unwrap_or(false);
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(!big)
            .truncate(big)
            .open(p);
        match f {
            Ok(file) => glidekm_core::log::set_file(file),
            Err(e) => warn!("cannot open log file {}: {e}", p.display()),
        }
    }

    let path = flags
        .get("config")
        .map(PathBuf::from)
        .unwrap_or_else(|| exe_dir().join("glidekm-server.conf"));
    let mut file_cfg = match Config::load(&path) {
        Ok(c) => {
            info!("config: {}", path.display());
            c
        }
        Err(_) => {
            info!(
                "no config yet at {} - use the settings page",
                path.display()
            );
            Config::default()
        }
    };
    // Command-line values override the file (and are saved if the UI saves).
    file_cfg.apply_args(&args);
    let cfg = ServerConfig::from_config(&file_cfg).unwrap_or_else(|e| {
        warn!("config: {e} - using defaults for invalid values");
        ServerConfig::default()
    });

    let ctl = Control::new(path, cfg, net::Shared::new());
    let opts = Opts { flags };
    #[cfg(windows)]
    let r = win::run(ctl, opts);
    #[cfg(not(windows))]
    let r = sim::run(ctl, opts);
    if let Err(e) = r {
        warn!("{e}");
        std::process::exit(1);
    }
}
