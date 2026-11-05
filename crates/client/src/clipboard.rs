//! Clipboard sharing (text and PNG images) on the Deck. There is no clipboard
//! library here: the system's own tools are used (`wl-clipboard` on Wayland,
//! `xclip` or `xsel` on X11/Xwayland), found at run time. A background thread
//! polls the clipboard, reports changes, and applies content received from
//! the PC.

use glidekm_core::clip::{self, Content, Watcher};
use glidekm_core::{debug, info, warn};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Clip {
    /// Content received from the PC, to put on the clipboard.
    pub set: Sender<Content>,
    /// Local clipboard changes, to send to the PC.
    pub changed: Receiver<Content>,
    /// Whether a PC is connected (changes made while offline are not sent).
    pub connected: Arc<AtomicBool>,
}

impl Clip {
    pub fn spawn() -> Clip {
        let (set, set_rx) = mpsc::channel();
        let (out_tx, changed) = mpsc::channel();
        let connected = Arc::new(AtomicBool::new(false));
        let c = connected.clone();
        let _ = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || run(set_rx, out_tx, c));
        Clip {
            set,
            changed,
            connected,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tool {
    WlClipboard,
    Xclip,
    Xsel,
}

fn run(set_rx: Receiver<Content>, out: Sender<Content>, connected: Arc<AtomicBool>) {
    let mut w = Watcher::default();
    let mut tool: Option<Tool> = None;
    let mut warned = false;
    let mut cache = ReadCache::default();
    loop {
        match set_rx.recv_timeout(Duration::from_millis(400)) {
            Ok(c) => {
                w.set_remote(&c);
                if let Some(tl) = tool.or_else(detect) {
                    if write(tl, &c) {
                        tool = Some(tl);
                        debug!("clipboard: received {} bytes", c.size());
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if tool.is_none() {
                    tool = detect();
                    match tool {
                        Some(t) => info!("clipboard sharing via {t:?}"),
                        None if !warned => {
                            warned = true;
                            info!(
                                "clipboard sharing needs wl-clipboard (Wayland) or xclip/xsel (X11) - none found"
                            );
                        }
                        None => {}
                    }
                }
                let Some(tl) = tool else { continue };
                if let Some(cur) = read(tl, &mut cache) {
                    if w.changed(&cur, connected.load(Ordering::Relaxed)) {
                        debug!("clipboard: sending {} bytes", cur.size());
                        if out.send(cur).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .chain(["/usr/bin", "/usr/local/bin", "/bin"].map(Into::into))
        .any(|d| d.join(name).is_file())
}

/// First tool installed for the current session (an empty clipboard makes
/// the tools fail, so they can't be probed by reading).
fn detect() -> Option<Tool> {
    let env = session_env();
    let has_wl = env.iter().any(|(k, _)| *k == "WAYLAND_DISPLAY");
    let has_x = env.iter().any(|(k, _)| *k == "DISPLAY");
    let mut order = Vec::new();
    if has_wl && which("wl-paste") && which("wl-copy") {
        order.push(Tool::WlClipboard);
    }
    if has_x && which("xclip") {
        order.push(Tool::Xclip);
    }
    if has_x && which("xsel") {
        order.push(Tool::Xsel);
    }
    order.into_iter().next()
}

/// Display variables for the desktop session. The systemd user service does
/// not inherit them, so fall back to looking for the sockets.
fn session_env() -> Vec<(&'static str, String)> {
    let mut v = Vec::new();
    let uid = unsafe { libc::getuid() };
    let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    v.push(("XDG_RUNTIME_DIR", rt.clone()));
    if let Ok(w) = std::env::var("WAYLAND_DISPLAY") {
        v.push(("WAYLAND_DISPLAY", w));
    } else if let Ok(rd) = std::fs::read_dir(&rt) {
        let mut socks: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
            .collect();
        socks.sort();
        if let Some(s) = socks.into_iter().next() {
            v.push(("WAYLAND_DISPLAY", s));
        }
    }
    if let Ok(d) = std::env::var("DISPLAY") {
        v.push(("DISPLAY", d));
    } else if let Ok(rd) = std::fs::read_dir("/tmp/.X11-unix") {
        let mut xs: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with('X'))
            .collect();
        xs.sort();
        if let Some(x) = xs.first() {
            v.push(("DISPLAY", format!(":{}", &x[1..])));
        }
    }
    v
}

/// MIME types currently offered by the clipboard owner (empty if none).
fn types(t: Tool) -> String {
    let args: &[&str] = match t {
        Tool::WlClipboard => &["wl-paste", "--list-types"],
        Tool::Xclip => &["xclip", "-selection", "clipboard", "-t", "TARGETS", "-o"],
        Tool::Xsel => return String::new(),
    };
    run_cmd(args, None, 64 * 1024)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// What the file list looked like at the last look, so unchanged copied
/// files are not read (and sent) again on every poll.
#[derive(Default)]
struct ReadCache {
    uris: String,
}

fn read(t: Tool, cache: &mut ReadCache) -> Option<Content> {
    let ty = types(t);
    let has = |m: &str| ty.lines().any(|l| l == m);
    if has("text/uri-list") && t != Tool::Xsel {
        let args: &[&str] = match t {
            Tool::WlClipboard => &["wl-paste", "--type", "text/uri-list"],
            _ => &[
                "xclip",
                "-selection",
                "clipboard",
                "-t",
                "text/uri-list",
                "-o",
            ],
        };
        let b = run_cmd(args, None, 256 * 1024)?;
        let uris = String::from_utf8(b).ok()?;
        if uris == cache.uris {
            return None;
        }
        cache.uris = uris.clone();
        let paths = parse_uri_list(&uris);
        if paths.is_empty() {
            return None;
        }
        return match clip::load_files(&paths) {
            Some(f) => Some(Content::Files(f)),
            None => {
                warn!("clipboard: files not shared (folders, or over 32 MB in total)");
                None
            }
        };
    }
    cache.uris.clear();
    let has_text = ty
        .lines()
        .any(|l| l.starts_with("text/plain") || l == "UTF8_STRING" || l == "STRING" || l == "TEXT");
    let has_png = has("image/png");
    // Text first (copying cells or rich text offers an image too).
    if has_text || t == Tool::Xsel || !has_png {
        let args: &[&str] = match t {
            Tool::WlClipboard => &["wl-paste", "--no-newline", "--type", "text/plain"],
            Tool::Xclip => &["xclip", "-selection", "clipboard", "-o"],
            Tool::Xsel => &["xsel", "--clipboard", "--output"],
        };
        let b = run_cmd(args, None, clip::MAX_TEXT)?;
        let s = String::from_utf8(b).ok()?;
        return (!s.is_empty()).then(|| Content::Text(clip::normalize(&s)));
    }
    let args: &[&str] = match t {
        Tool::WlClipboard => &["wl-paste", "--type", "image/png"],
        _ => &["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
    };
    let b = run_cmd(args, None, clip::MAX_IMAGE)?;
    (b.len() > 4 && b[0] == 0x89 && &b[1..4] == b"PNG").then_some(Content::Png(b))
}

fn parse_uri_list(s: &str) -> Vec<PathBuf> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("file://")?;
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            let bytes = rest.as_bytes();
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    if let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            Some(PathBuf::from(String::from_utf8(out).ok()?))
        })
        .collect()
}

fn uri_encode(p: &Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// Where received files go (emptied before each new transfer).
fn inbox() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("glidekm").join("clip")
}

fn write(t: Tool, c: &Content) -> bool {
    let (args, data): (&[&str], Vec<u8>) = match (t, c) {
        (Tool::WlClipboard, Content::Text(s)) => (
            &["wl-copy", "--type", "text/plain;charset=utf-8"],
            s.clone().into_bytes(),
        ),
        (Tool::WlClipboard, Content::Png(b)) => (&["wl-copy", "--type", "image/png"], b.clone()),
        (Tool::Xclip, Content::Text(s)) => (
            &["xclip", "-selection", "clipboard", "-i"],
            s.clone().into_bytes(),
        ),
        (Tool::Xclip, Content::Png(b)) => (
            &["xclip", "-selection", "clipboard", "-t", "image/png", "-i"],
            b.clone(),
        ),
        (Tool::Xsel, Content::Text(s)) => {
            (&["xsel", "--clipboard", "--input"], s.clone().into_bytes())
        }
        (Tool::Xsel, _) => return false, // xsel can't do images or files
        (_, Content::Files(f)) => {
            let dir = inbox();
            let _ = std::fs::remove_dir_all(&dir);
            let paths = match clip::save_files(&dir, f) {
                Ok(p) => p,
                Err(e) => {
                    warn!("clipboard: cannot save received files: {e}");
                    return false;
                }
            };
            let list: String = paths
                .iter()
                .map(|p| {
                    uri_encode(p)
                        + "
"
                })
                .collect();
            let args: &[&str] = if t == Tool::WlClipboard {
                &["wl-copy", "--type", "text/uri-list"]
            } else {
                &[
                    "xclip",
                    "-selection",
                    "clipboard",
                    "-t",
                    "text/uri-list",
                    "-i",
                ]
            };
            (args, list.into_bytes())
        }
    };
    run_cmd(args, Some(&data), 0).is_some()
}

/// Run a clipboard tool, feeding `input` and returning up to `max` bytes of
/// stdout. Bounded by a timeout so a stuck tool can't hang us.
fn run_cmd(args: &[&str], input: Option<&[u8]>, max: usize) -> Option<Vec<u8>> {
    let reading = input.is_none();
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .envs(session_env())
        .stdin(if reading {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(if reading {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(data).ok()?;
        // dropped here: EOF, so the tool takes ownership of the selection
    }
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.take(max as u64 + 1).read_to_end(&mut buf);
        }
        buf
    });
    let start = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(4) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    (ok && (!reading || bytes.len() <= max)).then_some(bytes)
}
