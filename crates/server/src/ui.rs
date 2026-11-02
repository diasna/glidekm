//! Local settings UI: a tiny HTTP server on 127.0.0.1 serving one embedded
//! page plus a JSON API. Every API call needs the per-run token (sent by the
//! page as `X-Token`), and the Host header must be loopback, so other web
//! pages and DNS-rebinding tricks cannot reconfigure the server.

use crate::control::{side_name, Control, ServerConfig, Status};
use crate::layout::Side;
use glidekm_core::{debug, info, warn};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE: &str = include_str!("ui.html");
pub const DEFAULT_UI_PORT: u16 = 24242;

/// Start the UI server; returns the URL to open (includes the token).
pub fn spawn(ctl: Arc<Control>, port: u16) -> std::io::Result<String> {
    let listener =
        TcpListener::bind(("127.0.0.1", port)).or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/?t={}", ctl.token);
    *ctl.ui_url.lock().unwrap() = url.clone();
    std::thread::Builder::new()
        .name("ui".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let ctl = ctl.clone();
                // Short-lived connections; a thread each keeps a slow scan from
                // blocking status polling.
                let _ = std::thread::Builder::new()
                    .name("ui-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle(stream, &ctl, port) {
                            debug!("ui: {e}");
                        }
                    });
            }
        })?;
    info!("settings UI on http://127.0.0.1:{port}/");
    Ok(url)
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: String,
}

fn read_request(stream: &TcpStream) -> std::io::Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if headers.len() > 64 {
            return Err(std::io::Error::other("too many headers"));
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > 16 * 1024 {
        return Err(std::io::Error::other("body too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    Ok(Request {
        method,
        path: path.to_string(),
        query: parse_form(q),
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn respond(mut s: &TcpStream, code: u16, ctype: &str, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        s,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
         connect-src 'self'; img-src data:; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body.as_bytes())?;
    s.flush()
}

fn json_err(s: &TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    respond(
        s,
        code,
        "application/json",
        &format!("{{\"error\":{}}}", js(msg)),
    )
}

fn handle(stream: TcpStream, ctl: &Control, port: u16) -> std::io::Result<()> {
    let req = read_request(&stream)?;
    let host = req.headers.get("host").map(|h| h.as_str()).unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return respond(&stream, 403, "text/plain", "forbidden");
    }
    if req.method == "GET" && req.path == "/" {
        if req.query.get("t").map(|t| t.as_str()) != Some(ctl.token.as_str()) {
            return respond(
                &stream,
                403,
                "text/html; charset=utf-8",
                "<!doctype html><meta charset=utf-8><title>glidekm</title>\
                 <p style='font:16px system-ui;margin:3em'>Open the settings from the glidekm tray icon \
                 (this link expired when the server restarted).</p>",
            );
        }
        return respond(
            &stream,
            200,
            "text/html; charset=utf-8",
            &PAGE.replace("__TOKEN__", &ctl.token),
        );
    }
    if !req.path.starts_with("/api/") {
        return respond(&stream, 404, "text/plain", "not found");
    }
    if req.headers.get("x-token").map(|t| t.as_str()) != Some(ctl.token.as_str()) {
        return json_err(&stream, 403, "bad token");
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/state") => respond(&stream, 200, "application/json", &state_json(ctl)),
        ("GET", "/api/log") => {
            let since = req
                .query
                .get("since")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let (lines, next) = glidekm_core::log::recent(since);
            let arr: Vec<String> = lines.iter().map(|(_, l)| js(l)).collect();
            respond(
                &stream,
                200,
                "application/json",
                &format!("{{\"next\":{next},\"lines\":[{}]}}", arr.join(",")),
            )
        }
        ("POST", "/api/config") => {
            let f = parse_form(&req.body);
            match config_from_form(ctl, &f).and_then(|c| ctl.apply(c)) {
                Ok(()) => respond(&stream, 200, "application/json", "{\"ok\":true}"),
                Err(e) => json_err(&stream, 400, &e),
            }
        }
        ("POST", "/api/autostart") => {
            let on = parse_form(&req.body)
                .get("on")
                .map(|v| v == "1")
                .unwrap_or(false);
            match ctl.platform.lock().unwrap().set_autostart(on) {
                Ok(()) => respond(&stream, 200, "application/json", "{\"ok\":true}"),
                Err(e) => json_err(&stream, 400, &e),
            }
        }
        ("POST", "/api/switch") => {
            ctl.platform.lock().unwrap().toggle_switch();
            respond(&stream, 200, "application/json", "{\"ok\":true}")
        }
        ("POST", "/api/scan") => {
            let found = scan(ctl);
            let items: Vec<String> = found
                .iter()
                .map(|(a, n)| format!("{{\"addr\":{},\"name\":{}}}", js(a), js(n)))
                .collect();
            respond(
                &stream,
                200,
                "application/json",
                &format!("[{}]", items.join(",")),
            )
        }
        _ => json_err(&stream, 404, "unknown endpoint"),
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

/// application/x-www-form-urlencoded / query string decoding.
pub fn parse_form(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((decode(k)?, decode(v)?))
        })
        .collect()
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// JSON string literal.
pub fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '<' => o.push_str("\\u003c"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn form_and_json() {
        let f = parse_form("a=1&b=hello+w%C3%B6rld&c=%3D%26&d");
        assert_eq!(f["b"], "hello wörld");
        assert_eq!(f["c"], "=&");
        assert_eq!(f["d"], "");
        assert!(parse_form("x=%zz").is_empty());
        assert_eq!(js("a\"b\\c\n</script>"), r#""a\"b\\c\n\u003c/script>""#);
    }
}
