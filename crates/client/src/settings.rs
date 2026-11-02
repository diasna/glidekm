//! Client configuration model and the hub shared between the network loop
//! and the settings UI.

use glidekm_core::config::Config;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    pub passphrase: String,
    pub port: u16,
    /// 0 = auto-detect
    pub width: u16,
    pub height: u16,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            passphrase: String::new(),
            port: glidekm_core::DEFAULT_PORT,
            width: 0,
            height: 0,
        }
    }
}

impl ClientConfig {
    pub fn from_config(c: &Config) -> Result<ClientConfig, String> {
        let cfg = ClientConfig {
            passphrase: c.str_or("passphrase", ""),
            port: c.num::<u16>("port")?.unwrap_or(glidekm_core::DEFAULT_PORT),
            width: c.num::<u16>("width")?.unwrap_or(0),
            height: c.num::<u16>("height")?.unwrap_or(0),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn fixed_size(&self) -> Option<(u16, u16)> {
        (self.width > 0 && self.height > 0).then_some((self.width, self.height))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("port must be 1-65535".into());
        }
        if (self.width == 0) != (self.height == 0) {
            return Err("set both width and height, or neither (auto)".into());
        }
        if self.width != 0 && (self.width < 320 || self.height < 200) {
            return Err("screen size too small".into());
        }
        if self.passphrase.contains(['\r', '\n']) {
            return Err("passphrase must be a single line".into());
        }
        Ok(())
    }

    pub fn to_file(&self) -> String {
        let mut s = format!(
            "# glidekm client configuration (written by the settings page)\n\
             passphrase = \"{}\"\nport = {}\n",
            self.passphrase, self.port
        );
        if let Some((w, h)) = self.fixed_size() {
            s += &format!("width = {w}\nheight = {h}\n");
        }
        s
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        write_private(path, &self.to_file())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// Write a file readable only by its owner (it contains the passphrase).
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())
}

/// Live state published by the network loop for the UI.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub server: Option<String>,
    pub active: bool,
    pub capture: bool,
    pub width: u16,
    pub height: u16,
    pub listening: bool,
    pub net_error: String,
    pub input_error: String,
    pub last_rejected: String,
}

pub struct Hub {
    pub path: PathBuf,
    pub cfg: Mutex<ClientConfig>,
    pub snap: Mutex<Snapshot>,
    /// Set by the UI after saving; the network loop re-reads the config.
    pub reload: AtomicBool,
    pub token: String,
}

impl Hub {
    pub fn new(path: PathBuf, cfg: ClientConfig) -> Hub {
        Hub {
            path,
            cfg: Mutex::new(cfg),
            snap: Mutex::new(Snapshot::default()),
            reload: AtomicBool::new(false),
            token: glidekm_core::http::new_token(),
        }
    }

    pub fn apply(&self, new: ClientConfig) -> Result<(), String> {
        new.validate()?;
        new.save(&self.path)?;
        *self.cfg.lock().unwrap() = new;
        self.reload.store(true, Ordering::SeqCst);
        glidekm_core::info!("settings saved to {}", self.path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_validation() {
        let c = ClientConfig {
            passphrase: " a \"b\" #c ".into(),
            port: 5000,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            ClientConfig::from_config(&Config::parse(&c.to_file())).unwrap(),
            c
        );
        let auto = ClientConfig::default();
        assert_eq!(
            ClientConfig::from_config(&Config::parse(&auto.to_file())).unwrap(),
            auto
        );
        assert!(ClientConfig {
            width: 100,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(ClientConfig {
            port: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
    }
}
