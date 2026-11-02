//! Server settings UI: the embedded page plus its JSON API, served by
//! `glidekm_core::http` on 127.0.0.1 (token + Host checks live there).

use crate::control::{side_name, Control, ServerConfig, Status};
use crate::layout::Side;
use glidekm_core::http::{self, js, Request, Response};
use glidekm_core::{debug, info, warn};
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE: &str = include_str!("ui.html");
pub const DEFAULT_UI_PORT: u16 = 24242;

/// Start the UI server; returns the URL to open (includes the token).
pub fn spawn(ctl: Arc<Control>, port: u16) -> std::io::Result<String> {
    let c = ctl.clone();
    let port = http::spawn(
        port,
        ctl.token.clone(),
        PAGE,
        Arc::new(move |r: &Request| api(&c, r)),
    )?;
    let url = http::url(port, &ctl.token);
    *ctl.ui_url.lock().unwrap() = url.clone();
    info!("settings UI on http://127.0.0.1:{port}/");
    Ok(url)
}

fn api(ctl: &Control, req: &Request) -> Response {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/state") => Response::json(state_json(ctl)),
        ("GET", "/api/log") => Response::json(http::log_json(
            req.query
                .get("since")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        )),
        ("POST", "/api/config") => {
            match config_from_form(ctl, &req.form()).and_then(|c| ctl.apply(c)) {
                Ok(()) => Response::ok(),
                Err(e) => Response::err(400, &e),
            }
        }
        ("POST", "/api/autostart") => {
            let on = req.form().get("on").is_some_and(|v| v == "1");
            match ctl.platform.lock().unwrap().set_autostart(on) {
                Ok(()) => Response::ok(),
                Err(e) => Response::err(400, &e),
            }
        }
        ("POST", "/api/switch") => {
            ctl.platform.lock().unwrap().toggle_switch();
            Response::ok()
        }
        ("POST", "/api/scan") => {
            let items: Vec<String> = scan(ctl)
                .iter()
                .map(|(a, n)| format!("{{\"addr\":{},\"name\":{}}}", js(a), js(n)))
                .collect();
            Response::json(format!("[{}]", items.join(",")))
        }
        _ => Response::err(404, "unknown endpoint"),
    }
}

fn config_from_form(ctl: &Control, f: &HashMap<String, String>) -> Result<ServerConfig, String> {
    let cur = ctl.cfg.lock().unwrap().clone();
    let get = |k: &str| f.get(k).map(|s| s.trim().to_string());
    let side_s = get("side").unwrap_or_else(|| side_name(cur.side).into());
    Ok(ServerConfig {
        client: get("client").unwrap_or(cur.client.clone()),
        // Empty passphrase field = keep the current one (it is never sent to the page).
        passphrase: match f.get("passphrase") {
            Some(p) if !p.is_empty() => p.clone(),
            _ => cur.passphrase.clone(),
        },
        side: Side::parse(&side_s).ok_or("invalid side")?,
        offset: match get("offset") {
            Some(v) => v.parse::<f64>().map_err(|_| "invalid offset")?.round() as i32,
            None => cur.offset,
        },
        scale: match get("scale") {
            Some(v) => v.parse().map_err(|_| "invalid scale")?,
            None => cur.scale,
        },
        hotkey_switch: get("hotkey_switch").unwrap_or(cur.hotkey_switch.clone()),
        hotkey_capture: get("hotkey_capture").unwrap_or(cur.hotkey_capture.clone()),
        ignore_injected: get("ignore_injected")
            .map(|v| v == "1")
            .unwrap_or(cur.ignore_injected),
    })
}

fn state_json(ctl: &Control) -> String {
    let cfg = ctl.cfg.lock().unwrap().clone();
    let sh = &ctl.shared;
    let status = match ctl.status() {
        Status::Unconfigured => "unconfigured",
        Status::Connecting => "connecting",
        Status::Online => "online",
        Status::Active => "active",
    };
    let peer = sh
        .peer
        .lock()
        .unwrap()
        .map(|a| js(&a.to_string()))
        .unwrap_or("null".into());
    let size = sh
        .size()
        .map(|(w, h)| format!("[{w},{h}]"))
        .unwrap_or("null".into());
    let mons: Vec<String> = ctl
        .monitors
        .lock()
        .unwrap()
        .iter()
        .map(|m| format!("[{},{},{},{}]", m.l, m.t, m.r, m.b))
        .collect();
    let autostart = match ctl.platform.lock().unwrap().autostart() {
        Some(b) => b.to_string(),
        None => "null".into(),
    };
    format!(
        "{{\"version\":{},\"status\":\"{status}\",\"status_text\":{},\"peer\":{peer},\"client_size\":{size},\
         \"capture\":{},\"monitors\":[{}],\"autostart\":{autostart},\"config_path\":{},\
         \"config\":{{\"client\":{},\"side\":\"{}\",\"offset\":{},\"scale\":{},\"hotkey_switch\":{},\
         \"hotkey_capture\":{},\"ignore_injected\":{},\"passphrase_set\":{}}}}}",
        js(env!("CARGO_PKG_VERSION")),
        js(&ctl.status_text()),
        sh.capture.load(Ordering::Relaxed),
        mons.join(","),
        js(&ctl.path.display().to_string()),
        js(&cfg.client),
        side_name(cfg.side),
        cfg.offset,
        cfg.scale,
        js(&cfg.hotkey_switch),
        js(&cfg.hotkey_capture),
        cfg.ignore_injected,
        !cfg.passphrase.is_empty(),
    )
}

/// Broadcast a discovery probe and collect answering clients.
fn scan(ctl: &Control) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return found;
    };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let mut probe = vec![glidekm_core::session::PKT_DISCOVER];
    probe.extend_from_slice(glidekm_core::session::DISCOVER_MAGIC);
    let mut ports = vec![glidekm_core::DEFAULT_PORT];
    if let Some(p) = ctl
        .cfg
        .lock()
        .unwrap()
        .client
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
    {
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    let mut targets = vec!["255.255.255.255".to_string()];
    // Directed broadcast for the primary interface's /24 (some routers and
    // Windows setups only forward that one).
    if let Ok(probe_sock) = UdpSocket::bind("0.0.0.0:0") {
        if probe_sock.connect("192.0.2.1:9").is_ok() {
            if let Ok(std::net::SocketAddr::V4(a)) = probe_sock.local_addr() {
                let o = a.ip().octets();
                targets.push(format!("{}.{}.{}.255", o[0], o[1], o[2]));
            }
        }
    }
    let t0 = Instant::now();
    let mut buf = [0u8; 256];
    let mut sent = 0;
    while t0.elapsed() < Duration::from_millis(900) {
        // Send the probe a few times; UDP broadcasts get lost on Wi-Fi.
        if sent < 3 && t0.elapsed() >= Duration::from_millis(250 * sent) {
            sent += 1;
            for t in &targets {
                for p in &ports {
                    if let Err(e) = sock.send_to(&probe, (t.as_str(), *p)) {
                        debug!("scan send to {t}:{p}: {e}");
                    }
                }
            }
        }
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            let pkt = &buf[..n];
            if pkt.first() == Some(&glidekm_core::session::PKT_DISCOVER_REPLY)
                && pkt.get(1..9) == Some(&glidekm_core::session::DISCOVER_REPLY_MAGIC[..])
            {
                let name: String = String::from_utf8_lossy(&pkt[9..])
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(64)
                    .collect();
                let addr = if from.port() == glidekm_core::DEFAULT_PORT {
                    from.ip().to_string()
                } else {
                    from.to_string()
                };
                if !found.iter().any(|(a, _)| *a == addr) {
                    found.push((addr, name));
                }
            }
        }
    }
    if found.is_empty() {
        warn!("scan: no Deck answered (is the client running and on the same network?)");
    } else {
        info!(
            "scan: found {}",
            found
                .iter()
                .map(|(a, n)| format!("{n} ({a})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    found
}
