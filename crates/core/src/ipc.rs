//! File-based IPC between the Linux client daemon and its settings app.
//!
//! - Settings: the app edits the config file; the daemon notices the change
//!   (mtime) and applies it live.
//! - Status: the daemon rewrites a small `key = value` status file in the
//!   per-user runtime directory once a second.
//! - Log: the daemon mirrors its log into the runtime directory.
//!
//! Everything lives in directories only the user can read.

use crate::config::Config;
use std::io::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- paths

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_RUNTIME_DIR/glidekm` (tmpfs, per user).
pub fn runtime_dir() -> PathBuf {
    env_dir("XDG_RUNTIME_DIR")
        .unwrap_or_else(|| std::env::temp_dir().join(format!("glidekm-{}", uid())))
        .join("glidekm")
}

pub fn status_path() -> PathBuf {
    runtime_dir().join("status")
}

/// The settings app creates this file to ask the client to check for updates now.
pub fn update_request_path() -> PathBuf {
    runtime_dir().join("update-request")
}

pub fn log_path() -> PathBuf {
    runtime_dir().join("client.log")
}

/// `$XDG_CONFIG_HOME/glidekm/client.conf`.
pub fn client_config_path() -> PathBuf {
    env_dir("XDG_CONFIG_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("glidekm")
        .join("client.conf")
}

#[cfg(unix)]
fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}
#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

/// Create `dir` (and parents) readable only by the owner.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Write `text` to `path` atomically (temp file + rename), owner-only.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut o = std::fs::OpenOptions::new();
        o.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(text.as_bytes())?;
    }
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------- client config

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    pub passphrase: String,
    pub port: u16,
    /// 0 = auto-detect
    pub width: u16,
    pub height: u16,
    /// Install signed updates from GitHub Releases automatically.
    pub auto_update: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            passphrase: String::new(),
            port: crate::DEFAULT_PORT,
            width: 0,
            height: 0,
            auto_update: true,
        }
    }
}

impl ClientConfig {
    pub fn from_config(c: &Config) -> Result<ClientConfig, String> {
        let cfg = ClientConfig {
            passphrase: c.str_or("passphrase", ""),
            port: c.num::<u16>("port")?.unwrap_or(crate::DEFAULT_PORT),
            width: c.num::<u16>("width")?.unwrap_or(0),
            height: c.num::<u16>("height")?.unwrap_or(0),
            auto_update: c.get("auto_update").is_none_or(|_| c.flag("auto_update")),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<ClientConfig, String> {
        match Config::load(path) {
            Ok(c) => Self::from_config(&c),
            Err(_) => Ok(ClientConfig::default()),
        }
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
            "# glidekm client configuration (written by glidekm Settings)\n\
             passphrase = \"{}\"\nport = {}\nauto_update = {}\n",
            self.passphrase, self.port, self.auto_update
        );
        if let Some((w, h)) = self.fixed_size() {
            s += &format!("width = {w}\nheight = {h}\n");
        }
        s
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        write_private(path, &self.to_file())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

// ---------------------------------------------------------------- status

/// Daemon state as seen by the settings app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    /// Milliseconds since the epoch of the last write (liveness).
    pub updated: u64,
    pub listening: bool,
    pub port: u16,
    pub server: String,
    pub active: bool,
    pub capture: bool,
    pub width: u16,
    pub height: u16,
    pub screen_auto: bool,
    pub passphrase_set: bool,
    pub net_error: String,
    pub input_error: String,
    pub last_rejected: String,
    /// Human-readable update state ("" = nothing to report).
    pub update_status: String,
    /// Version of a newer release, if one was found.
    pub update_available: String,
    /// Updates can't be installed for this installation (e.g. Decky).
    pub update_unsupported: bool,
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

impl Status {
    pub fn to_file(&self) -> String {
        format!(
            "pid = {}\nversion = {}\nupdated = {}\nlistening = {}\nport = {}\nserver = {}\n\
             active = {}\ncapture = {}\nwidth = {}\nheight = {}\nscreen_auto = {}\n\
             passphrase_set = {}\nnet_error = {}\ninput_error = {}\nlast_rejected = {}\n\
             update_status = {}\nupdate_available = {}\nupdate_unsupported = {}\n",
            self.pid,
            one_line(&self.version),
            self.updated,
            self.listening,
            self.port,
            one_line(&self.server),
            self.active,
            self.capture,
            self.width,
            self.height,
            self.screen_auto,
            self.passphrase_set,
            one_line(&self.net_error),
            one_line(&self.input_error),
            one_line(&self.last_rejected),
            one_line(&self.update_status),
            one_line(&self.update_available),
            self.update_unsupported,
        )
    }

    pub fn parse(text: &str) -> Status {
        let c = Config::parse(text);
        let n = |k: &str| c.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        Status {
            pid: n("pid") as u32,
            version: c.str_or("version", ""),
            updated: n("updated"),
            listening: c.flag("listening"),
            port: n("port") as u16,
            server: c.str_or("server", ""),
            active: c.flag("active"),
            capture: c.flag("capture"),
            width: n("width") as u16,
            height: n("height") as u16,
            screen_auto: c.flag("screen_auto"),
            passphrase_set: c.flag("passphrase_set"),
            net_error: c.str_or("net_error", ""),
            input_error: c.str_or("input_error", ""),
            last_rejected: c.str_or("last_rejected", ""),
            update_status: c.str_or("update_status", ""),
            update_available: c.str_or("update_available", ""),
            update_unsupported: c.flag("update_unsupported"),
        }
    }

    /// Read the daemon's status; None if missing or stale (daemon not running).
    pub fn read_live() -> Option<Status> {
        let s = Status::parse(&std::fs::read_to_string(status_path()).ok()?);
        let age = crate::session::now_millis().saturating_sub(s.updated);
        (s.updated != 0 && age < 3500).then_some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_and_validation() {
        let c = ClientConfig {
            passphrase: " a \"b\" #c ".into(),
            port: 5000,
            width: 1920,
            height: 1080,
            auto_update: false,
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
    }

    #[test]
    fn status_roundtrip() {
        let s = Status {
            pid: 42,
            version: "0.1.0".into(),
            updated: 123,
            listening: true,
            port: 4242,
            server: "10.0.0.2:5555".into(),
            active: true,
            capture: false,
            width: 1280,
            height: 800,
            screen_auto: true,
            passphrase_set: true,
            net_error: String::new(),
            input_error: "cannot open /dev/uinput: denied".into(),
            last_rejected: "x\ny".into(),
            update_status: "Up to date".into(),
            update_available: "0.2.0".into(),
            update_unsupported: false,
        };
        let back = Status::parse(&s.to_file());
        assert_eq!(back.last_rejected, "x y");
        assert_eq!(
            back,
            Status {
                last_rejected: "x y".into(),
                ..s
            }
        );
    }

    #[test]
    fn atomic_private_write() {
        let dir = std::env::temp_dir().join(format!("glidekm-ipc-test-{}", std::process::id()));
        let p = dir.join("sub").join("f");
        write_private(&p, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
