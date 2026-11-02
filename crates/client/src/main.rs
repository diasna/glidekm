//! glidekm-client: receives input from the Windows server and injects it via
//! /dev/uinput. Works in SteamOS Game Mode (gamescope) and Desktop Mode.

mod screen;
mod sink;
mod state;
mod uinput;

use glidekm_core::config::Config;
use glidekm_core::event::{encode_all, Event};
use glidekm_core::session::{self, Transport, PKT_DATA, PKT_INIT};
use glidekm_core::{debug, info, warn};
use sink::{Recorder, Sink};
use state::State;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str =
    "glidekm-client [--config FILE] [--passphrase P] [--port N] [--width W --height H]
                [--status-file FILE] [--dry-run] [--verbose]
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
    let mut cli = Config::default();
    cli.apply_args(&args);
    let path = cli
        .get("config")
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    let mut cfg = match Config::load(&path) {
        Ok(c) => {
            info!("config: {}", path.display());
            c
        }
        Err(_) => Config::default(),
    };
    cfg.apply_args(&args);
    glidekm_core::log::set_verbose(cfg.flag("verbose"));

    let pass = cfg
        .get("passphrase")
        .ok_or("no passphrase set (config 'passphrase = ...' or --passphrase)")?;
    let psk = session::derive_psk(pass);
    let port = cfg
        .num::<u16>("port")?
        .unwrap_or(glidekm_core::DEFAULT_PORT);
    let fixed = match (cfg.num::<u16>("width")?, cfg.num::<u16>("height")?) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    };
    let (w, h) = fixed.or_else(screen::detect).unwrap_or((1280, 800));
    info!(
        "screen {w}x{h}{}",
        if fixed.is_some() {
            " (from config)"
        } else {
            ""
        }
    );
    let status_file = cfg.get("status_file").map(PathBuf::from);

    let sock = UdpSocket::bind(("0.0.0.0", port)).map_err(|e| format!("bind udp {port}: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(100))).ok();
    info!("listening on udp {port}");

    if cfg.flag("dry_run") {
        let rec = Recorder {
            echo: true,
            ..Default::default()
        };
        serve(
            sock,
            psk,
            State::new(rec, w, h),
            fixed.is_none(),
            status_file,
        )
    } else {
        let sink = sink::Uinput::new().map_err(|e| {
            format!(
                "cannot create uinput devices: {e}\n  check /dev/uinput access (see README: udev rule or run as root)"
            )
        })?;
        info!("uinput devices created");
        serve(
            sock,
            psk,
            State::new(sink, w, h),
            fixed.is_none(),
            status_file,
        )
    }
}

struct Peer {
    addr: SocketAddr,
    tr: Transport,
    last_rx: Instant,
    last_hello: Option<Instant>,
}

fn serve<S: Sink>(
    sock: UdpSocket,
    psk: [u8; 32],
    mut st: State<S>,
    autodetect: bool,
    status_file: Option<PathBuf>,
) -> Result<(), String> {
    let mut peer: Option<Peer> = None;
    let mut last_ts = 0u64;
    let mut buf = vec![0u8; 2048];
    let mut last_detect = Instant::now();
    let mut status = String::new();

    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let pkt = &buf[..n];
                match pkt.first() {
                    Some(&PKT_INIT) => match session::respond(&psk, pkt, &mut last_ts) {
                        Ok((tr, resp)) => {
                            let _ = sock.send_to(&resp, from);
                            if peer.as_ref().map(|p| p.addr) != Some(from) {
                                info!("server connected: {from}");
                            } else {
                                debug!("session renewed with {from}");
                            }
                            st.deactivate();
                            peer = Some(Peer {
                                addr: from,
                                tr,
                                last_rx: Instant::now(),
                                last_hello: None,
                            });
                            send_hello(&sock, peer.as_mut().unwrap(), &st);
                        }
                        Err(e) => debug!("rejected handshake from {from}: {e}"),
                    },
                    Some(&PKT_DATA) => {
                        let Some(p) = peer.as_mut().filter(|p| p.addr == from) else {
                            continue;
                        };
                        match p.tr.open(pkt) {
                            Ok(plain) => {
                                p.last_rx = Instant::now();
                                let mut heartbeat = false;
                                for ev in Event::decode_all(&plain) {
                                    debug!("rx {ev:?}");
                                    heartbeat |= matches!(ev, Event::Heartbeat { .. });
                                    st.handle(ev);
                                }
                                st.sink.flush();
                                if heartbeat
                                    && p.last_hello
                                        .is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
                                {
                                    send_hello(&sock, p, &st);
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
                        send_hello(&sock, p, &st);
                    }
                }
            }
        }

        if let Some(path) = &status_file {
            let s = format!(
                "{{\"connected\":{},\"server\":\"{}\",\"active\":{},\"capture\":{},\"width\":{},\"height\":{}}}\n",
                peer.is_some(),
                peer.as_ref().map(|p| p.addr.to_string()).unwrap_or_default(),
                st.active,
                st.capture,
                st.width,
                st.height
            );
            if s != status {
                let _ = std::fs::write(path, &s);
                status = s;
            }
        }
    }
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
