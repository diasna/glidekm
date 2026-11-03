//! Live state shared inside the client daemon, published to the runtime
//! status file for glidekm Settings (and Decky), plus config-file watching.

use glidekm_core::config::Config;
use glidekm_core::ipc::{self, ClientConfig, Status};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Watches the config file; returns a new config when it changed on disk.
pub struct ConfigWatch {
    path: PathBuf,
    args: Vec<String>,
    mtime: Option<SystemTime>,
}

impl ConfigWatch {
    pub fn new(path: PathBuf, args: Vec<String>) -> Self {
        let mtime = mtime(&path);
        ConfigWatch { path, args, mtime }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the config file with command-line overrides applied.
    pub fn load(&self) -> Result<ClientConfig, String> {
        let mut c = Config::load(&self.path).unwrap_or_default();
        c.apply_args(&self.args);
        ClientConfig::from_config(&c)
    }

    /// Some(config) if the file changed since the last call and parses.
    pub fn poll(&mut self) -> Option<ClientConfig> {
        let m = mtime(&self.path);
        if m == self.mtime {
            return None;
        }
        self.mtime = m;
        match self.load() {
            Ok(c) => Some(c),
            Err(e) => {
                glidekm_core::warn!("config change ignored: {e}");
                None
            }
        }
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Write the status file(s). `json` = legacy JSON file for the Decky plugin.
pub fn publish(st: &Status, json: Option<&PathBuf>) {
    if let Err(e) = ipc::write_private(&ipc::status_path(), &st.to_file()) {
        glidekm_core::debug!("status write: {e}");
    }
    if let Some(p) = json {
        let j = format!(
            "{{\"connected\":{},\"server\":\"{}\",\"active\":{},\"capture\":{},\"width\":{},\"height\":{},\"input_error\":\"{}\"}}\n",
            !st.server.is_empty(),
            st.server.replace(['"', '\\'], ""),
            st.active,
            st.capture,
            st.width,
            st.height,
            st.input_error.replace(['"', '\\'], "")
        );
        let _ = std::fs::write(p, j);
    }
}
