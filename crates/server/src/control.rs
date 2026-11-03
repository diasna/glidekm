//! Server configuration model and the control hub shared by the settings UI,
//! the tray and the input engine. Platform independent.

use crate::hotkey::Hotkey;
use crate::layout::{Rect, Side};
use crate::net::Shared;
use glidekm_core::config::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub client: String,
    pub passphrase: String,
    pub side: Side,
    pub offset: i32,
    pub scale: f64,
    pub hotkey_switch: String,
    pub hotkey_capture: String,
    pub ignore_injected: bool,
    /// Install signed updates from GitHub Releases automatically.
    pub auto_update: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            client: String::new(),
            passphrase: String::new(),
            side: Side::Right,
            offset: 0,
            scale: 1.0,
            hotkey_switch: "ctrl+alt+shift+s".into(),
            hotkey_capture: "ctrl+alt+shift+m".into(),
            ignore_injected: false,
            auto_update: true,
        }
    }
}

/// Parsed, validated values the engine needs.
#[derive(Debug, Clone, Copy)]
pub struct EngineParams {
    pub side: Side,
    pub offset: i32,
    pub scale: f64,
    pub hk_switch: Hotkey,
    pub hk_capture: Hotkey,
}

pub fn side_name(s: Side) -> &'static str {
    match s {
        Side::Left => "left",
        Side::Right => "right",
        Side::Top => "top",
        Side::Bottom => "bottom",
    }
}

impl ServerConfig {
    pub fn from_config(c: &Config) -> Result<ServerConfig, String> {
        let d = ServerConfig::default();
        let side_s = c.str_or("side", "right");
        let cfg = ServerConfig {
            client: c.str_or("client", ""),
            passphrase: c.str_or("passphrase", ""),
            side: Side::parse(&side_s).ok_or(format!("invalid side '{side_s}'"))?,
            offset: c.num::<i32>("offset")?.unwrap_or(0),
            scale: c.num::<f64>("scale")?.unwrap_or(1.0),
            hotkey_switch: c.str_or("hotkey_switch", &d.hotkey_switch),
            hotkey_capture: c.str_or("hotkey_capture", &d.hotkey_capture),
            ignore_injected: c.flag("ignore_injected"),
            auto_update: c.get("auto_update").is_none_or(|_| c.flag("auto_update")),
        };
        cfg.engine_params()?;
        Ok(cfg)
    }

    /// Only the passphrase is required: with no address the Deck is found
    /// automatically on the local network.
    pub fn configured(&self) -> bool {
        !self.passphrase.is_empty()
    }

    pub fn engine_params(&self) -> Result<EngineParams, String> {
        if !(0.1..=10.0).contains(&self.scale) {
            return Err("scale must be between 0.1 and 10".into());
        }
        if self.offset.abs() > 100_000 {
            return Err("offset out of range".into());
        }
        let hk_switch = Hotkey::parse(&self.hotkey_switch)?;
        let hk_capture = Hotkey::parse(&self.hotkey_capture)?;
        if hk_switch == hk_capture {
            return Err("the two hotkeys must differ".into());
        }
        Ok(EngineParams {
            side: self.side,
            offset: self.offset,
            scale: self.scale,
            hk_switch,
            hk_capture,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        self.engine_params()?;
        let c = self.client.trim();
        if c.contains(char::is_whitespace) || c.contains(['/', '?', '#', '@']) {
            return Err("Deck address must be a host name or IP, optionally with :port".into());
        }
        if self.passphrase.contains(['\r', '\n']) {
            return Err("passphrase must be a single line".into());
        }
        Ok(())
    }

    pub fn to_file(&self) -> String {
        format!(
            "# glidekm server configuration (written by the settings UI)\n\
             client = {}\n\
             passphrase = \"{}\"\n\
             side = {}\n\
             offset = {}\n\
             scale = {}\n\
             hotkey_switch = {}\n\
             hotkey_capture = {}\n\
             ignore_injected = {}\n\
             auto_update = {}\n",
            self.client.trim(),
            self.passphrase,
            side_name(self.side),
            self.offset,
            self.scale,
            self.hotkey_switch,
            self.hotkey_capture,
            self.ignore_injected,
            self.auto_update
        )
    }
}

/// What the OS-specific side provides to the control hub.
pub trait Platform: Send + Sync {
    /// Ask the input thread to re-read the config (engine params etc).
    fn config_changed(&self);
    /// Ask the input thread to toggle between PC and Deck.
    fn toggle_switch(&self) {}
    /// Start with Windows; None = not supported here.
    fn autostart(&self) -> Option<bool> {
        None
    }
    fn set_autostart(&self, _on: bool) -> Result<(), String> {
        Err("not supported on this platform".into())
    }
}

pub struct NoPlatform;
impl Platform for NoPlatform {
    fn config_changed(&self) {}
}

/// Self-update state shown in the settings window.
#[derive(Debug, Default, Clone)]
pub struct UpdateInfo {
    pub status: String,
    /// Newer version found, if any.
    pub available: String,
    /// Version downloaded, verified and waiting to be installed.
    pub staged: Option<String>,
}

pub struct Control {
    pub path: PathBuf,
    pub cfg: Mutex<ServerConfig>,
    pub shared: Arc<Shared>,
    pub monitors: Mutex<Vec<Rect>>,
    pub platform: Mutex<Box<dyn Platform>>,
    pub update: Mutex<UpdateInfo>,
    /// Set by the settings window: check (and install) updates now.
    pub update_request: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unconfigured,
    Connecting,
    Online,
    Active,
}

impl Control {
    pub fn new(path: PathBuf, cfg: ServerConfig, shared: Arc<Shared>) -> Arc<Control> {
        let c = Arc::new(Control {
            path,
            cfg: Mutex::new(cfg),
            shared,
            monitors: Mutex::new(Vec::new()),
            platform: Mutex::new(Box::new(NoPlatform)),
            update: Mutex::new(UpdateInfo::default()),
            update_request: AtomicBool::new(false),
        });
        c.push_target();
        c
    }

    fn push_target(&self) {
        let cfg = self.cfg.lock().unwrap().clone();
        if cfg.configured() {
            self.shared.set_target(
                cfg.client.trim(),
                glidekm_core::session::derive_psk(&cfg.passphrase),
            );
        }
    }

    pub fn status(&self) -> Status {
        if !self.shared.configured() {
            Status::Unconfigured
        } else if !self.shared.alive.load(Ordering::Relaxed) {
            Status::Connecting
        } else if self.shared.active.load(Ordering::Relaxed) {
            Status::Active
        } else {
            Status::Online
        }
    }

    pub fn status_text(&self) -> String {
        let peer = self.shared.peer.lock().unwrap().map(|a| a.to_string());
        match self.status() {
            Status::Unconfigured => "Not set up - choose a passphrase".into(),
            Status::Connecting => {
                let e = self.shared.last_error.lock().unwrap().clone();
                if !e.is_empty() {
                    e
                } else if self.shared.auto.load(Ordering::Relaxed) {
                    "Looking for your Steam Deck on this network…".into()
                } else {
                    format!(
                        "Connecting to {}…",
                        peer.unwrap_or_else(|| self.cfg.lock().unwrap().client.clone())
                    )
                }
            }
            Status::Online => {
                let name = self.shared.peer_name.lock().unwrap().clone();
                let peer = peer.unwrap_or_default();
                if name.is_empty() {
                    format!("Connected to {peer}")
                } else {
                    format!("Connected to {name} ({peer})")
                }
            }
            Status::Active => "Deck in use".into(),
        }
    }

    /// Validate, persist and apply a new configuration.
    pub fn apply(&self, new: ServerConfig) -> Result<(), String> {
        new.validate()?;
        std::fs::write(&self.path, new.to_file())
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))?;
        *self.cfg.lock().unwrap() = new;
        self.push_target();
        self.platform.lock().unwrap().config_changed();
        glidekm_core::info!("settings saved to {}", self.path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_through_file() {
        let c = ServerConfig {
            client: "10.0.0.5:4242".into(),
            passphrase: "a b=c#d".into(),
            side: Side::Top,
            offset: -120,
            scale: 1.5,
            ignore_injected: true,
            auto_update: false,
            ..Default::default()
        };
        let back = ServerConfig::from_config(&Config::parse(&c.to_file())).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn validation() {
        let mut c = ServerConfig {
            client: "deck".into(),
            passphrase: "x".into(),
            ..Default::default()
        };
        assert!(c.validate().is_ok());
        c.hotkey_capture = c.hotkey_switch.clone();
        assert!(c.validate().is_err());
        c = ServerConfig {
            client: "a b".into(),
            ..Default::default()
        };
        assert!(c.validate().is_err());
        c = ServerConfig {
            scale: 50.0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        assert!(!ServerConfig::default().configured());
    }
}
