//! glidekm-client: receives input from the Windows server and injects it via
//! /dev/uinput. Works in SteamOS Game Mode (gamescope) and Desktop Mode.

mod screen;
mod settings;
mod sink;
mod state;
mod ui;
mod uinput;

use glidekm_core::config::Config;
use glidekm_core::event::{encode_all, Event};
use glidekm_core::session::{self, Transport, PKT_DATA, PKT_DISCOVER, PKT_INIT};
use glidekm_core::{debug, info, warn};
use settings::{ClientConfig, Hub};
use sink::{Output, Recorder, RetryUinput, Sink};
use state::State;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str =
    "glidekm-client [--config FILE] [--passphrase P] [--port N] [--width W --height H]
                [--status-file FILE] [--ui-port N] [--no-ui] [--dry-run] [--verbose]
glidekm-client --open-settings     open the settings page (starts the client if needed)
Config file default: $XDG_CONFIG_HOME/glidekm/client.conf (~/.config/glidekm/client.conf)";

fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("glidekm").join("client.conf")
}

fn main() {
    if let Err(e) = run() {
        warn!("{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("glidekm-client {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut flags = Config::default();
    flags.apply_args(&args);
    if flags.flag("open_settings") {
        return ui::open_settings();
    }
    glidekm_core::log::set_verbose(flags.flag("verbose"));

    let path = flags
        .get("config")
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    let mut file = match Config::load(&path) {
        Ok(c) => {
            info!("config: {}", path.display());
            c
        }
        Err(_) => {
            info!("no config yet at {}", path.display());
            Config::default()
        }
    };
    file.apply_args(&args);
    let cfg = ClientConfig::from_config(&file).unwrap_or_else(|e| {
        warn!("config: {e} - using defaults");
        ClientConfig::default()
    });
    if cfg.passphrase.is_empty() {
        warn!("no passphrase set - open the settings page (menu: glidekm Settings) to set one");
    }
    let hub = Arc::new(Hub::new(path, cfg));
    if !flags.flag("no_ui") {
        let port = flags.num::<u16>("ui_port")?.unwrap_or(ui::DEFAULT_UI_PORT);
        match ui::spawn(hub.clone(), port) {
            Ok(url) => debug!("settings url: {url}"),
            Err(e) => warn!("settings page unavailable: {e}"),
        }
    }
    let status_file = flags.get("status_file").map(PathBuf::from);

    let out = if flags.flag("dry_run") {
        Output::Dry(Recorder {
            echo: true,
            ..Default::default()
        })
    } else {
        Output::Real(RetryUinput::new())
    };
    let mut st = State::new(out, 1280, 800);

    // Each iteration (re)applies the configuration; `serve` returns when the
    // settings page saved new values.
    loop {
        hub.reload.store(false, Ordering::SeqCst);
        let cfg = hub.cfg.lock().unwrap().clone();
        let fixed = cfg.fixed_size();
        let (w, h) = fixed.or_else(screen::detect).unwrap_or((1280, 800));
        st.deactivate();
        st.width = w;
        st.height = h;
        info!(
            "screen {w}x{h}{}",
            if fixed.is_some() {
                " (from config)"
            } else {
                ""
            }
        );
        let psk = (!cfg.passphrase.is_empty()).then(|| session::derive_psk(&cfg.passphrase));
        let sock = match UdpSocket::bind(("0.0.0.0", cfg.port)) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!(
                    "cannot listen on UDP {}: {e} (is another glidekm client, e.g. the Decky plugin, running?)",
                    cfg.port
                );
                warn!("{msg}");
                {
                    let mut s = hub.snap.lock().unwrap();
                    s.net_error = msg;
                    s.listening = false;
                }
                // Retry, but react to new settings right away.
                for _ in 0..20 {
                    if hub.reload.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
        };
        sock.set_read_timeout(Some(Duration::from_millis(100))).ok();
        info!("listening on udp {}", cfg.port);
        {
            let mut s = hub.snap.lock().unwrap();
            s.net_error.clear();
            s.listening = true;
        }
        serve(
            &sock,
            psk,
            &mut st,
            fixed.is_none(),
            status_file.as_ref(),
            &hub,
        );
        info!("applying new settings");
    }
}

struct Peer {
    addr: SocketAddr,
    tr: Transport,
    last_rx: Instant,
    last_hello: Option<Instant>,
}

fn serve(
    sock: &UdpSocket,
    psk: Option<[u8; 32]>,
    st: &mut State<Output>,
    autodetect: bool,
    status_file: Option<&PathBuf>,
    hub: &Hub,
) {
    let mut peer: Option<Peer> = None;
    let mut last_ts = 0u64;
    let mut buf = vec![0u8; 2048];
    let mut last_detect = Instant::now();
    let mut last_publish = Instant::now() - Duration::from_secs(1);
    let mut status = String::new();
    let mut rejected = String::new();

    while !hub.reload.load(Ordering::SeqCst) {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let pkt = &buf[..n];
                match pkt.first() {
                    Some(&PKT_DISCOVER) if pkt.get(1..9) == Some(&session::DISCOVER_MAGIC[..]) => {
                        let mut r = vec![session::PKT_DISCOVER_REPLY];
                        r.extend_from_slice(session::DISCOVER_REPLY_MAGIC);
                        r.extend_from_slice(hostname().as_bytes());
                        let _ = sock.send_to(&r, from);
                    }
                    Some(&PKT_INIT) => {
                        let Some(psk) = psk.as_ref() else {
                            rejected = format!(
                                "{} tried to connect, but no passphrase is set here",
                                from.ip()
                            );
                            continue;
                        };
                        match session::respond(psk, pkt, &mut last_ts) {
                            Ok((tr, resp)) => {
                                let _ = sock.send_to(&resp, from);
                                if peer.as_ref().map(|p| p.addr) != Some(from) {
                                    info!("server connected: {from}");
                                } else {
                                    debug!("session renewed with {from}");
                                }
                                rejected.clear();
                                st.deactivate();
                                peer = Some(Peer {
                                    addr: from,
                                    tr,
                                    last_rx: Instant::now(),
                                    last_hello: None,
                                });
                                send_hello(sock, peer.as_mut().unwrap(), st);
                            }
                            Err(session::Error::Replay) => debug!("stale handshake from {from}"),
                            Err(e) => {
                                debug!("rejected handshake from {from}: {e}");
                                rejected = format!(
                                    "{} tried to connect with a different passphrase",
                                    from.ip()
                                );
                            }
                        }
                    }
                    Some(&PKT_DATA) => {
                        let Some(p) = peer.as_mut().filter(|p| p.addr == from) else {
                            continue;
                        };
                        match p.tr.open(pkt) {
                            Ok(plain) => {
                                p.last_rx = Instant::now();
                                let mut heartbeat = false;
                                for ev in Event::decode_all(&plain) {
                                    if matches!(ev, Event::Heartbeat { .. }) {
                                        heartbeat = true;
                                    } else {
                                        debug!("rx {ev:?}");
                                    }
                                    st.handle(ev);
                                }
                                st.sink.flush();
                                if heartbeat
                                    && p.last_hello
                                        .is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
                                {
                                    send_hello(sock, p, st);
                                }
                            }
                            Err(e) => debug!("drop packet: {e}"),
                        }
                    }
                    _ => {}
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => warn!("recv: {e}"),
        }

        // Watchdog: never leave keys held if the server vanishes.
        if let Some(p) = &peer {
            if p.last_rx.elapsed() > Duration::from_millis(glidekm_core::SERVER_TIMEOUT_MS)
                && st.active
            {
                warn!("server silent, releasing input");
                st.deactivate();
            }
            if p.last_rx.elapsed() > Duration::from_secs(10) {
                info!("server {} gone", p.addr);
                peer = None;
            }
        }

        // Dock/undock changes the screen; re-check every few seconds.
        if autodetect && last_detect.elapsed() > Duration::from_secs(5) {
            last_detect = Instant::now();
            if let Some((w, h)) = screen::detect() {
                if (w, h) != (st.width, st.height) {
                    info!("screen changed to {w}x{h}");
                    st.width = w;
                    st.height = h;
                    if let Some(p) = peer.as_mut() {
                        send_hello(sock, p, st);
                    }
                }
            }
        }

        // Publish state for the settings page / Decky (at most 10x per second).
        if last_publish.elapsed() >= Duration::from_millis(100) {
            last_publish = Instant::now();
            st.sink.tick();
            {
                let mut s = hub.snap.lock().unwrap();
                s.server = peer.as_ref().map(|p| p.addr.to_string());
                s.active = st.active;
                s.capture = st.capture;
                s.width = st.width;
                s.height = st.height;
                s.input_error = st.sink.error().to_string();
                s.last_rejected = rejected.clone();
            }
            if let Some(path) = status_file {
                let s = format!(
                    "{{\"connected\":{},\"server\":\"{}\",\"active\":{},\"capture\":{},\"width\":{},\"height\":{},\"input_error\":{}}}\n",
                    peer.is_some(),
                    peer.as_ref().map(|p| p.addr.to_string()).unwrap_or_default(),
                    st.active,
                    st.capture,
                    st.width,
                    st.height,
                    glidekm_core::http::js(st.sink.error())
                );
                if s != status {
                    let _ = std::fs::write(path, &s);
                    status = s;
                }
            }
        }
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().chars().take(64).collect())
        .unwrap_or_default()
}

fn send_hello<S: Sink>(sock: &UdpSocket, p: &mut Peer, st: &State<S>) {
    let msg = encode_all(&[Event::Hello {
        width: st.width,
        height: st.height,
    }]);
    if let Ok(pkt) = p.tr.seal(&msg) {
        let _ = sock.send_to(&pkt, p.addr);
        p.last_hello = Some(Instant::now());
    }
}
