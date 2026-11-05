//! Text clipboard sharing on the Deck. There is no clipboard library here:
//! the system's own tools are used (`wl-clipboard` on Wayland, `xclip` or
//! `xsel` on X11/Xwayland), found at run time. A background thread polls the
//! clipboard, reports changes, and applies text received from the PC.

use glidekm_core::clip::{self, Watcher};
use glidekm_core::{debug, info};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Clip {
    /// Text received from the PC, to put on the clipboard.
    pub set: Sender<String>,
    /// Local clipboard changes, to send to the PC.
    pub changed: Receiver<String>,
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

fn run(set_rx: Receiver<String>, out: Sender<String>, connected: Arc<AtomicBool>) {
    let mut w = Watcher::default();
    let mut tool: Option<Tool> = None;
    let mut warned = false;
    loop {
        match set_rx.recv_timeout(Duration::from_millis(400)) {
            Ok(t) => {
                let t = clip::normalize(&t);
                w.set_remote(&t);
                if let Some(tl) = tool.or_else(detect) {
                    if write(tl, &t) {
                        tool = Some(tl);
                        debug!("clipboard: received {} bytes", t.len());
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
                if let Some(cur) = read(tl) {
                    let cur = clip::normalize(&cur);
                    if w.changed(&cur, connected.load(Ordering::Relaxed)) {
                        debug!("clipboard: sending {} bytes", cur.len());
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

fn read(t: Tool) -> Option<String> {
    run_cmd(t, true, None)
}

fn write(t: Tool, text: &str) -> bool {
    run_cmd(t, false, Some(text)).is_some()
}

/// Run the tool for reading (`input` None) or writing; reading returns the
/// text. Everything is bounded by a timeout so a stuck tool can't hang us.
fn run_cmd(t: Tool, reading: bool, input: Option<&str>) -> Option<String> {
    let (prog, args): (&str, &[&str]) = match (t, reading) {
        (Tool::WlClipboard, true) => ("wl-paste", &["--no-newline", "--type", "text/plain"]),
        (Tool::WlClipboard, false) => ("wl-copy", &["--type", "text/plain;charset=utf-8"]),
        (Tool::Xclip, true) => ("xclip", &["-selection", "clipboard", "-o"]),
        (Tool::Xclip, false) => ("xclip", &["-selection", "clipboard", "-i"]),
        (Tool::Xsel, true) => ("xsel", &["--clipboard", "--output"]),
        (Tool::Xsel, false) => ("xsel", &["--clipboard", "--input"]),
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .envs(session_env())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if reading { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes()).ok()?;
        // dropped here: EOF, so the tool takes ownership of the selection
    }
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.take(clip::MAX_BYTES as u64 + 1).read_to_end(&mut buf);
        }
        buf
    });
    let start = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(2) => {
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
    if !ok {
        return None;
    }
    if reading {
        let s = String::from_utf8(bytes).ok()?;
        (s.len() <= clip::MAX_BYTES).then_some(s)
    } else {
        Some(String::new())
    }
}
