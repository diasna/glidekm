//! Windows self-update: WinHTTP download (system TLS + proxy settings, no
//! extra libraries), signed-manifest verification, and exe swap + restart.

use crate::control::{Control, Status};
use glidekm_core::update::{self, Fetch};
use glidekm_core::{info, warn};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const WM_APP_INSTALL_UPDATE: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 30;
const FIRST_CHECK: Duration = Duration::from_secs(90);
const INTERVAL: Duration = Duration::from_secs(6 * 3600);
const MAX_DOWNLOAD: usize = 64 << 20;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub struct WinHttp;

struct Handle(*mut core::ffi::c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn last_err(what: &str) -> String {
    format!("{what} failed (error {})", unsafe { GetLastError() })
}

impl Fetch for WinHttp {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        unsafe {
            let wurl = wide(url);
            let mut uc: URL_COMPONENTS = std::mem::zeroed();
            uc.dwStructSize = std::mem::size_of::<URL_COMPONENTS>() as u32;
            uc.dwHostNameLength = u32::MAX;
            uc.dwUrlPathLength = u32::MAX;
            uc.dwExtraInfoLength = u32::MAX;
            if WinHttpCrackUrl(wurl.as_ptr(), 0, 0, &mut uc) == 0 {
                return Err(last_err("parsing URL"));
            }
            let host: Vec<u16> =
                std::slice::from_raw_parts(uc.lpszHostName, uc.dwHostNameLength as usize)
                    .iter()
                    .copied()
                    .chain(Some(0))
                    .collect();
            let mut path: Vec<u16> =
                std::slice::from_raw_parts(uc.lpszUrlPath, uc.dwUrlPathLength as usize).to_vec();
            if uc.dwExtraInfoLength > 0 {
                path.extend_from_slice(std::slice::from_raw_parts(
                    uc.lpszExtraInfo,
                    uc.dwExtraInfoLength as usize,
                ));
            }
            path.push(0);
            let agent = wide(&format!("glidekm/{}", env!("CARGO_PKG_VERSION")));
            let session = Handle(WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            ));
            if session.0.is_null() {
                return Err(last_err("WinHttpOpen"));
            }
            WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 60_000);
            let conn = Handle(WinHttpConnect(session.0, host.as_ptr(), uc.nPort, 0));
            if conn.0.is_null() {
                return Err(last_err("connecting"));
            }
            let verb = wide("GET");
            let secure = uc.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;
            let req = Handle(WinHttpOpenRequest(
                conn.0,
                verb.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                if secure { WINHTTP_FLAG_SECURE } else { 0 },
            ));
            if req.0.is_null() {
                return Err(last_err("opening request"));
            }
            if WinHttpSendRequest(req.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0 {
                return Err(last_err("sending request"));
            }
            if WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0 {
                return Err(last_err("receiving response"));
            }
            let mut code: u32 = 0;
            let mut size = 4u32;
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut code as *mut u32 as *mut _,
                &mut size,
                std::ptr::null_mut(),
            );
            if code != 200 {
                return Err(format!(
                    "HTTP {code} for {}",
                    url.rsplit('/').next().unwrap_or(url)
                ));
            }
            let mut out = Vec::new();
            loop {
                let mut avail = 0u32;
                if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 {
                    return Err(last_err("reading"));
                }
                if avail == 0 {
                    break;
                }
                let start = out.len();
                out.resize(start + avail as usize, 0);
                let mut read = 0u32;
                if WinHttpReadData(req.0, out[start..].as_mut_ptr() as *mut _, avail, &mut read)
                    == 0
                {
                    return Err(last_err("reading"));
                }
                out.truncate(start + read as usize);
                if out.len() > MAX_DOWNLOAD {
                    return Err("download too large".into());
                }
            }
            Ok(out)
        }
    }
}

fn now_hm() -> String {
    let secs = glidekm_core::session::now_millis() / 1000;
    format!("{:02}:{:02} UTC", (secs / 3600) % 24, (secs / 60) % 60)
}

fn staged_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.with_extension("exe.new")
}

/// Background update thread. `main_hwnd` receives WM_APP_INSTALL_UPDATE
/// when a verified update is staged and the Deck is idle.
pub fn spawn(ctl: Arc<Control>, main_hwnd: isize) {
    if !update::enabled() {
        ctl.update.lock().unwrap().status = "Updates are not enabled in this build.".into();
        return;
    }
    let _ = std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            let mut next = Instant::now() + FIRST_CHECK;
            let mut posted = false;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let asked = ctl.update_request.swap(false, Ordering::SeqCst);
                let staged = ctl.update.lock().unwrap().staged.clone();
                if let Some(v) = staged {
                    // Install only while the Deck is not being used.
                    if !posted && ctl.status() != Status::Active {
                        info!("installing update {v}");
                        posted = true;
                        unsafe { PostMessageW(main_hwnd as _, WM_APP_INSTALL_UPDATE, 0, 0) };
                    }
                    continue;
                }
                if !asked && Instant::now() < next {
                    continue;
                }
                next = Instant::now() + INTERVAL;
                let auto = ctl.cfg.lock().unwrap().auto_update;
                check_and_stage(&ctl, asked || auto);
            }
        });
}

fn check_and_stage(ctl: &Control, install: bool) {
    let set = |s: String| ctl.update.lock().unwrap().status = s;
    set("Checking for updates…".into());
    let current = env!("CARGO_PKG_VERSION");
    match update::check(&WinHttp, current) {
        Ok(None) => {
            ctl.update.lock().unwrap().available.clear();
            set(format!("Up to date (v{current}) · checked {}", now_hm()));
        }
        Ok(Some(m)) => {
            ctl.update.lock().unwrap().available = m.version.clone();
            if !install {
                set(format!("Version {} is available.", m.version));
                return;
            }
            set(format!("Downloading {}…", m.version));
            let res = update::download(&WinHttp, &m, update::ASSET_SERVER_WIN)
                .and_then(|data| std::fs::write(staged_path(), data).map_err(|e| e.to_string()));
            match res {
                Ok(()) => {
                    info!("update {} downloaded and verified", m.version);
                    set(format!(
                        "Version {} ready - installs when the Deck is not in use.",
                        m.version
                    ));
                    ctl.update.lock().unwrap().staged = Some(m.version.clone());
                }
                Err(e) => {
                    let _ = std::fs::remove_file(staged_path());
                    warn!("update {}: {e}", m.version);
                    set(format!("Update failed: {e}"));
                }
            }
        }
        Err(e) => {
            warn!("update check: {e}");
            set(format!("Update check failed: {e}"));
        }
    }
}

/// Swap the staged exe in and start it. On success the caller quits.
pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = staged_path();
    let old = exe.with_extension("exe.old");
    if !new.exists() {
        return Err("no staged update".into());
    }
    let _ = std::fs::remove_file(&old);
    // A running exe can be renamed (not overwritten).
    std::fs::rename(&exe, &old).map_err(|e| format!("rename current exe: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("move new exe in place: {e}"));
    }
    let mut args: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--after-update" {
            it.next();
            continue;
        }
        args.push(a);
    }
    args.push("--after-update".into());
    args.push(std::process::id().to_string());
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(_) => Ok(()),
        Err(e) => {
            // Roll back so this installation keeps working.
            let _ = std::fs::rename(&exe, &new);
            let _ = std::fs::rename(&old, &exe);
            Err(format!("start new version: {e}"))
        }
    }
}

/// In the new process: wait for the previous one to exit, then clean up.
pub fn finish_after_update(old_pid: u32) {
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, old_pid);
        if !h.is_null() {
            WaitForSingleObject(h, 15_000);
            CloseHandle(h);
        }
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let old: PathBuf = exe.with_extension("exe.old");
    for _ in 0..20 {
        if !old.exists() || std::fs::remove_file(&old).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    info!("updated to {}", env!("CARGO_PKG_VERSION"));
}
