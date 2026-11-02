//! Desktop Mode settings page for the client: embedded HTML + JSON API on
//! 127.0.0.1 (via `glidekm_core::http`), a URL file so the menu launcher can
//! find the running instance, and the launcher itself (`--open-settings`).

use crate::settings::{ClientConfig, Hub};
use glidekm_core::http::{self, js, Request, Response};
use glidekm_core::{info, warn};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE: &str = include_str!("ui.html");
pub const DEFAULT_UI_PORT: u16 = 24243;
const UNIT: &str = "glidekm-client.service";

/// Where the running client publishes its settings URL (owner-only).
pub fn url_file() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("glidekm").join("settings-url")
}

pub fn spawn(hub: Arc<Hub>, port: u16) -> std::io::Result<String> {
    let h = hub.clone();
    let port = http::spawn(
        port,
        hub.token.clone(),
        PAGE,
        Arc::new(move |r: &Request| api(&h, r)),
    )?;
    let url = http::url(port, &hub.token);
    let f = url_file();
    if let Some(dir) = f.parent() {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    if let Err(e) = crate::settings::write_private(&f, &url) {
        warn!("cannot write {}: {e}", f.display());
    }
    info!("settings page on http://127.0.0.1:{port}/ (menu: glidekm Settings)");
    Ok(url)
}

fn api(hub: &Hub, req: &Request) -> Response {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/state") => Response::json(state_json(hub)),
        ("GET", "/api/log") => Response::json(http::log_json(
            req.query
                .get("since")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        )),
        ("POST", "/api/config") => {
            let f = req.form();
            let cur = hub.cfg.lock().unwrap().clone();
            let num = |k: &str, d: u16| -> Result<u16, String> {
                match f.get(k).map(|s| s.trim()) {
                    None => Ok(d),
                    Some("") => Ok(0),
                    Some(v) => v.parse().map_err(|_| format!("invalid {k}")),
                }
            };
            let new = (|| {
                Ok::<_, String>(ClientConfig {
                    passphrase: match f.get("passphrase") {
                        Some(p) if !p.is_empty() => p.clone(),
                        _ => cur.passphrase.clone(),
                    },
                    port: num("port", cur.port)?,
                    width: num("width", cur.width)?,
                    height: num("height", cur.height)?,
                })
            })();
            match new.and_then(|c| hub.apply(c)) {
                Ok(()) => Response::ok(),
                Err(e) => Response::err(400, &e),
            }
        }
        ("POST", "/api/autostart") => {
            let on = req.form().get("on").is_some_and(|v| v == "1");
            match set_autostart(on) {
                Ok(()) => Response::ok(),
                Err(e) => Response::err(400, &e),
            }
        }
        _ => Response::err(404, "unknown endpoint"),
    }
}

fn state_json(hub: &Hub) -> String {
    let cfg = hub.cfg.lock().unwrap().clone();
    let s = hub.snap.lock().unwrap().clone();
    let status = if cfg.passphrase.is_empty() {
        "unconfigured"
    } else if !s.input_error.is_empty() || !s.net_error.is_empty() {
        "problem"
    } else if s.active {
        "active"
    } else if s.server.is_some() {
        "online"
    } else {
        "waiting"
    };
    let ips: Vec<String> = local_ipv4s().iter().map(|ip| js(&ip.to_string())).collect();
    let autostart = match autostart() {
        Some(b) => b.to_string(),
        None => "null".into(),
    };
    format!(
        "{{\"version\":{},\"status\":\"{status}\",\"server\":{},\"active\":{},\"capture\":{},\
         \"screen\":[{},{}],\"hostname\":{},\"ips\":[{}],\"net_error\":{},\"input_error\":{},\
         \"last_rejected\":{},\"autostart\":{autostart},\"config_path\":{},\
         \"config\":{{\"port\":{},\"width\":{},\"height\":{},\"passphrase_set\":{}}}}}",
        js(env!("CARGO_PKG_VERSION")),
        s.server.as_deref().map(js).unwrap_or("null".into()),
        s.active,
        s.capture,
        s.width,
        s.height,
        js(&crate::hostname()),
        ips.join(","),
        js(&s.net_error),
        js(&s.input_error),
        js(&s.last_rejected),
        js(&hub.path.display().to_string()),
        cfg.port,
        cfg.width,
        cfg.height,
        !cfg.passphrase.is_empty(),
    )
}

/// Non-loopback IPv4 addresses of this machine.
pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() && !out.contains(&ip) {
                    out.push(ip);
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

// ---------------------------------------------------------------- autostart

fn unit_installed() -> bool {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.is_some_and(|b| b.join("systemd/user").join(UNIT).exists())
}

/// Some(enabled) when installed as a systemd user service, else None.
fn autostart() -> Option<bool> {
    if !unit_installed() {
        return None;
    }
    let out = Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim() == "enabled")
}

fn set_autostart(on: bool) -> Result<(), String> {
    if !unit_installed() {
        return Err("not installed as a service (run install.sh)".into());
    }
    let st = Command::new("systemctl")
        .args(["--user", if on { "enable" } else { "disable" }, UNIT])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        info!("start automatically: {}", if on { "on" } else { "off" });
        Ok(())
    } else {
        Err("systemctl failed".into())
    }
}

// ---------------------------------------------------------------- launcher

fn running_url() -> Option<String> {
    let url = std::fs::read_to_string(url_file()).ok()?.trim().to_string();
    let port: u16 = url
        .strip_prefix("http://127.0.0.1:")?
        .split('/')
        .next()?
        .parse()
        .ok()?;
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300))
        .ok()
        .map(|_| url)
}

fn ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn has_browser() -> bool {
    Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

/// `glidekm-client --open-settings`: make sure the client runs, then open
/// its settings page (default browser, else Steam's built-in browser).
pub fn open_settings() -> Result<(), String> {
    let url = match running_url() {
        Some(u) => u,
        None => {
            info!("client not running - starting it");
            if !(unit_installed() && ok("systemctl", &["--user", "start", UNIT])) {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                Command::new(exe)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| format!("cannot start client: {e}"))?;
            }
            let t0 = Instant::now();
            loop {
                if let Some(u) = running_url() {
                    break u;
                }
                if t0.elapsed() > Duration::from_secs(6) {
                    return Err("the client did not start; run `journalctl --user -u glidekm-client` for details".into());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    let steam_url = format!("steam://openurl/{url}");
    let opened = (has_browser() && ok("xdg-open", &[&url]))
        || Command::new("steam")
            .arg(&steam_url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        || ok("xdg-open", &[&url]);
    if !opened {
        println!("Open this address in a browser: {url}");
        let _ = ok(
            "kdialog",
            &[
                "--msgbox",
                &format!("Open this address in a browser:\n{url}"),
            ],
        );
    }
    Ok(())
}
