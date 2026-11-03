//! Background self-update for the Linux client (and the settings app next
//! to it): signed GitHub releases, fetched with curl. New binaries are
//! staged next to the old ones and swapped in while the Deck is idle; the
//! client then re-executes itself.

use glidekm_core::update::{self, Curl, Manifest};
use glidekm_core::{info, warn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FIRST_CHECK: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(6 * 3600);

#[derive(Default)]
pub struct State {
    pub status: String,
    pub available: String,
    pub unsupported: bool,
    /// (version, [(target, staged file)]) ready to install.
    staged: Option<(String, Vec<(PathBuf, PathBuf)>)>,
}

pub struct Updater {
    pub state: Arc<Mutex<State>>,
    pub auto: Arc<AtomicBool>,
    request: Arc<AtomicBool>,
}

/// Why this installation can't update itself, if it can't.
fn unsupported_reason(exe: &Path) -> Option<&'static str> {
    if !update::enabled() {
        return Some("Updates are not enabled in this build.");
    }
    let s = exe.to_string_lossy();
    if s.contains("/homebrew/plugins/") {
        return Some("Updates are handled by Decky.");
    }
    let dir = exe.parent()?;
    let probe = dir.join(".glidekm-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(probe);
            None
        }
        Err(_) => Some("Install folder is not writable - update by re-running install.sh."),
    }
}

impl Updater {
    pub fn spawn(auto: bool) -> Updater {
        let u = Updater {
            state: Arc::new(Mutex::new(State::default())),
            auto: Arc::new(AtomicBool::new(auto)),
            request: Arc::new(AtomicBool::new(false)),
        };
        let exe = std::env::current_exe().unwrap_or_default();
        if let Some(why) = unsupported_reason(&exe) {
            {
                let mut s = u.state.lock().unwrap();
                s.unsupported = true;
                s.status = why.into();
            }
            return u;
        }
        let (state, auto, request) = (u.state.clone(), u.auto.clone(), u.request.clone());
        let _ = std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || {
                let mut next = Instant::now() + FIRST_CHECK;
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let asked = request.swap(false, Ordering::SeqCst);
                    if !asked && Instant::now() < next {
                        continue;
                    }
                    next = Instant::now() + INTERVAL;
                    run_check(&state, &exe, asked || auto.load(Ordering::Relaxed));
                }
            });
        u
    }

    /// The settings app asked for a check (it creates a request file).
    pub fn request_now(&self) {
        self.request.store(true, Ordering::SeqCst);
    }

    /// Install a staged update if there is one. Only call while idle.
    /// On success this re-executes the process and does not return.
    pub fn install_if_staged(&self) {
        let staged = self.state.lock().unwrap().staged.take();
        let Some((version, files)) = staged else {
            return;
        };
        let mut done: Vec<&Path> = Vec::new();
        for (target, new) in &files {
            if let Err(e) = std::fs::rename(new, target) {
                warn!("update: cannot replace {}: {e}", target.display());
                self.state.lock().unwrap().status = format!("Update failed: {e}");
                return;
            }
            done.push(target);
        }
        info!("updated to {version} ({} files) - restarting", done.len());
        restart();
    }
}

fn now_hm() -> String {
    let secs = glidekm_core::session::now_millis() / 1000;
    format!("{:02}:{:02} UTC", (secs / 3600) % 24, (secs / 60) % 60)
}

fn run_check(state: &Mutex<State>, exe: &Path, install: bool) {
    state.lock().unwrap().status = "Checking for updates…".into();
    let current = env!("CARGO_PKG_VERSION");
    match update::check(&Curl, current) {
        Ok(None) => {
            let mut s = state.lock().unwrap();
            s.available.clear();
            s.status = format!("Up to date (v{current}) · checked {}", now_hm());
        }
        Ok(Some(m)) => {
            state.lock().unwrap().available = m.version.clone();
            if !install {
                state.lock().unwrap().status = format!("Version {} is available.", m.version);
                return;
            }
            match stage(&m, exe) {
                Ok(files) => {
                    info!("update {} downloaded and verified", m.version);
                    let mut s = state.lock().unwrap();
                    s.status = format!(
                        "Version {} ready - installs when the Deck is idle.",
                        m.version
                    );
                    s.staged = Some((m.version.clone(), files));
                }
                Err(e) => {
                    warn!("update {}: {e}", m.version);
                    state.lock().unwrap().status = format!("Update failed: {e}");
                }
            }
        }
        Err(e) => {
            warn!("update check: {e}");
            state.lock().unwrap().status = format!("Update check failed: {e}");
        }
    }
}

/// Download + verify the release binaries that exist in this installation.
fn stage(m: &Manifest, exe: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let dir = exe.parent().ok_or("no install folder")?;
    let mut targets = vec![(exe.to_path_buf(), update::ASSET_CLIENT_LINUX)];
    let settings = dir.join("glidekm-settings");
    if settings.exists() {
        targets.push((settings, update::ASSET_SETTINGS_LINUX));
    }
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    let res = (|| {
        for (target, asset) in targets {
            let data = update::download(&Curl, m, asset)?;
            let new = target.with_extension("new");
            write_exec(&new, &data).map_err(|e| format!("{}: {e}", new.display()))?;
            out.push((target, new));
        }
        Ok(())
    })();
    if let Err(e) = res {
        // All or nothing: don't leave half a release staged.
        for (_, new) in &out {
            let _ = std::fs::remove_file(new);
        }
        return Err(e);
    }
    Ok(out)
}

fn write_exec(p: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o755)
        .open(p)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Replace this process with the (new) binary, same arguments and PID, so a
/// systemd service keeps tracking it.
fn restart() -> ! {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    // current_exe() may report "(deleted)" after the rename; strip it.
    let exe = PathBuf::from(
        exe.to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_string(),
    );
    let err = std::process::Command::new(&exe)
        .args(std::env::args().skip(1))
        .exec();
    warn!("restart after update failed: {err}");
    std::process::exit(1);
}
