//! glidekm-server: shares this Windows PC's mouse and keyboard with a SteamOS
//! client that behaves like an extra monitor.

mod engine;
mod hotkey;
mod layout;
mod net;
#[cfg(not(windows))]
mod sim;
#[cfg(windows)]
mod win;

use glidekm_core::config::Config;
use glidekm_core::{info, warn};
use std::path::PathBuf;

pub const USAGE: &str = "glidekm-server [--config FILE] [--client HOST[:PORT]] [--passphrase P]
                [--side right|left|top|bottom] [--offset PX] [--scale F]
                [--hotkey-switch ctrl+alt+shift+s] [--hotkey-capture ctrl+alt+shift+m]
                [--background] [--log-file FILE] [--verbose]
Config file default: glidekm-server.conf next to the executable.";

pub struct Settings {
    pub client: String,
    pub psk: [u8; 32],
    pub side: layout::Side,
    pub offset: i32,
    pub scale: f64,
    pub hk_switch: hotkey::Hotkey,
    pub hk_capture: hotkey::Hotkey,
    pub background: bool,
    pub cfg: Config,
}

fn settings() -> Result<Option<Settings>, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(None);
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("glidekm-server {}", env!("CARGO_PKG_VERSION"));
        return Ok(None);
    }
    let mut cli = Config::default();
    cli.apply_args(&args);
    let path = cli.get("config").map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("glidekm-server.conf")))
            .unwrap_or_else(|| PathBuf::from("glidekm-server.conf"))
    });
    let mut cfg = Config::load(&path).unwrap_or_default();
    cfg.apply_args(&args);
    glidekm_core::log::set_verbose(cfg.flag("verbose"));
    if let Some(f) = cfg.get("log_file") {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(f)
        {
            Ok(file) => glidekm_core::log::set_file(file),
            Err(e) => warn!("cannot open log file {f}: {e}"),
        }
    }
    if path.exists() {
        info!("config: {}", path.display());
    }

    let client = cfg
        .get("client")
        .ok_or("no client address (config 'client = steamdeck.local' or --client)")?
        .to_string();
    let pass = cfg
        .get("passphrase")
        .ok_or("no passphrase set (config 'passphrase = ...' or --passphrase)")?;
    let side_s = cfg.str_or("side", "right");
    let side = layout::Side::parse(&side_s).ok_or(format!("invalid side '{side_s}'"))?;
    let scale = cfg.num::<f64>("scale")?.unwrap_or(1.0);
    if !(0.1..=10.0).contains(&scale) {
        return Err("scale must be between 0.1 and 10".into());
    }
    Ok(Some(Settings {
        psk: glidekm_core::session::derive_psk(pass),
        client,
        side,
        offset: cfg.num::<i32>("offset")?.unwrap_or(0),
        scale,
        hk_switch: hotkey::Hotkey::parse(&cfg.str_or("hotkey_switch", "ctrl+alt+shift+s"))?,
        hk_capture: hotkey::Hotkey::parse(&cfg.str_or("hotkey_capture", "ctrl+alt+shift+m"))?,
        background: cfg.flag("background"),
        cfg,
    }))
}

fn main() {
    let s = match settings() {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            warn!("{e}");
            eprintln!("\n{USAGE}");
            std::process::exit(2);
        }
    };
    #[cfg(windows)]
    let r = win::run(s);
    #[cfg(not(windows))]
    let r = sim::run(s);
    if let Err(e) = r {
        warn!("{e}");
        std::process::exit(1);
    }
}
