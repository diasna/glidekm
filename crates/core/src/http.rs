//! Minimal loopback HTTP server for the settings pages (no dependencies).
//!
//! Security model: listen on 127.0.0.1 only; the page URL carries a random
//! per-run token (`?t=`) that the page sends back as `X-Token` on every API
//! call; the Host header must be loopback (defeats DNS rebinding). A custom
//! header also forces a CORS preflight, which is never answered, so other web
//! pages cannot call the API.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Request {
    pub fn form(&self) -> HashMap<String, String> {
        parse_form(&self.body)
    }
}

pub struct Response {
    pub code: u16,
    pub ctype: &'static str,
    pub body: String,
}

impl Response {
    pub fn json(body: String) -> Response {
        Response {
            code: 200,
            ctype: "application/json",
            body,
        }
    }
    pub fn ok() -> Response {
        Response::json("{\"ok\":true}".into())
    }
    pub fn err(code: u16, msg: &str) -> Response {
        Response {
            code,
            ctype: "application/json",
            body: format!("{{\"error\":{}}}", js(msg)),
        }
    }
    pub fn html(code: u16, body: String) -> Response {
        Response {
            code,
            ctype: "text/html; charset=utf-8",
            body,
        }
    }
}

pub type Handler = dyn Fn(&Request) -> Response + Send + Sync;

/// Serves `page` (with `__TOKEN__` replaced) at `/?t=TOKEN` and passes
/// authenticated `/api/*` requests to `api`. Returns the bound port.
pub fn spawn(
    preferred_port: u16,
    token: String,
    page: &'static str,
    api: Arc<Handler>,
) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", preferred_port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new()
        .name("http".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let api = api.clone();
                let token = token.clone();
                // One short-lived thread per connection so a slow request
                // (LAN scan) never blocks status polling.
                let _ = std::thread::Builder::new()
                    .name("http-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle(&stream, port, &token, page, &*api) {
                            crate::debug!("http: {e}");
                        }
                    });
            }
        })?;
    Ok(port)
}

pub fn url(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/?t={token}")
}

/// A fresh random token (hex).
pub fn new_token() -> String {
    crate::session::random_bytes()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn handle(
    stream: &TcpStream,
    port: u16,
    token: &str,
    page: &str,
    api: &Handler,
) -> std::io::Result<()> {
    let req = read_request(stream)?;
    let host = req.headers.get("host").map(|h| h.as_str()).unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return write_response(stream, &Response::err(403, "forbidden"));
    }
    let resp = if req.method == "GET" && req.path == "/" {
        if req.query.get("t").map(|t| t.as_str()) == Some(token) {
            Response::html(200, page.replace("__TOKEN__", token))
        } else {
            Response::html(
                403,
                "<!doctype html><meta charset=utf-8><title>glidekm</title>\
                 <p style='font:16px system-ui;margin:3em'>This link has expired. Open the settings \
                 again from the glidekm icon or menu entry.</p>"
                    .into(),
            )
        }
    } else if !req.path.starts_with("/api/") {
        Response::err(404, "not found")
    } else if req.headers.get("x-token").map(|t| t.as_str()) != Some(token) {
        Response::err(403, "bad token")
    } else {
        api(&req)
    };
    write_response(stream, &resp)
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

fn write_response(mut s: &TcpStream, r: &Response) -> std::io::Result<()> {
    let reason = match r.code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        s,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
         connect-src 'self'; img-src data:; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        r.code,
        r.ctype,
        r.body.len()
    )?;
    s.write_all(r.body.as_bytes())?;
    s.flush()
}

/// Log lines since `since` as `{"next":N,"lines":[...]}`.
pub fn log_json(since: u64) -> String {
    let (lines, next) = crate::log::recent(since);
    let arr: Vec<String> = lines.iter().map(|(_, l)| js(l)).collect();
    format!("{{\"next\":{next},\"lines\":[{}]}}", arr.join(","))
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

/// JSON string literal (also safe inside HTML script blocks).
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
        let lt = format!("{}u003c", '\\');
        assert_eq!(
            js("a\"b\\c\n</script>"),
            format!("\"a\\\"b\\\\c\\n{lt}/script>\"")
        );
    }

    #[test]
    fn serves_with_token_and_host_checks() {
        let api: Arc<Handler> =
            Arc::new(|r: &Request| Response::json(format!("{{\"p\":{}}}", js(&r.path))));
        let port = spawn(0, "tok".into(), "<p>__TOKEN__</p>", api).unwrap();
        let get = |path: &str, host: &str, tok: Option<&str>| -> String {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let th = tok.map(|t| format!("X-Token: {t}\r\n")).unwrap_or_default();
            write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n{th}\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        let h = format!("127.0.0.1:{port}");
        assert!(get("/?t=tok", &h, None).contains("<p>tok</p>"));
        assert!(get("/?t=bad", &h, None).starts_with("HTTP/1.1 403"));
        assert!(get("/?t=tok", "evil.example:80", None).starts_with("HTTP/1.1 403"));
        assert!(get("/api/x", &h, None).starts_with("HTTP/1.1 403"));
        assert!(get("/api/x", &h, Some("tok")).contains("{\"p\":\"/api/x\"}"));
    }
}
