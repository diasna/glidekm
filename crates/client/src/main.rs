//! glidekm-client: receives input from the Windows server and injects it via
//! /dev/uinput. Works in SteamOS Game Mode (gamescope) and Desktop Mode.
//!
//! Settings come from the config file (edited by glidekm Settings, watched
//! and applied live); state is published to the runtime status file.

mod screen;
mod settings;
mod sink;
mod state;
mod uinput;
mod updater;

use glidekm_core::config::Config;
use glidekm_core::event::{encode_all, Event};
use glidekm_core::ipc::{self, ClientConfig, Status};
use glidekm_core::session::{self, Transport, PKT_DATA, PKT_DISCOVER, PKT_INIT};
use glidekm_core::{debug, discovery, info, warn};
use settings::ConfigWatch;
use sink::{Output, Recorder, RetryUinput, Sink};
use state::State;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str =
    "glidekm-client [--config FILE] [--passphrase P] [--port N] [--width W --height H]
                [--status-file FILE] [--dry-run] [--verbose]
Settings are normally changed with the glidekm Settings app (or the Decky plugin).
Config file default: $XDG_CONFIG_HOME/glidekm/client.conf (~/.config/glidekm/client.conf)";

fn main() {
    if let Err(e) = run() {
        warn!("{e}");
        std::process::exit(1);
    }
}

/// Everything the receive loop needs besides the socket.
struct Daemon {
    st: State<Output>,
    watch: ConfigWatch,
    cfg: ClientConfig,
    status: Status,
    json_status: Option<PathBuf>,
    updater: updater::Updater,
    screens: screen::Detector,
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
    glidekm_core::log::set_verbose(flags.flag("verbose"));
    // Mirror the log for glidekm Settings (appended; restarted when > 1 MB).
    if ipc::private_dir(&ipc::runtime_dir()).is_ok() {
        let p = ipc::log_path();
        let big = std::fs::metadata(&p)
            .map(|m| m.len() > 1 << 20)
            .unwrap_or(false);
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(!big)
            .truncate(big)
            .open(&p);
        if let Ok(f) = f {
            glidekm_core::log::set_file(f);
        }
    }

    let path = flags
        .get("config")
        .map(PathBuf::from)
        .unwrap_or_else(ipc::client_config_path);
    info!(
        "glidekm-client {} | config {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
    let watch = ConfigWatch::new(path, args);
    let cfg = watch.load().unwrap_or_else(|e| {
        warn!("config: {e} - using defaults");
        ClientConfig::default()
    });
    if cfg.passphrase.is_empty() {
        warn!("no passphrase set - open glidekm Settings to set one");
    }

    let out = if flags.flag("dry_run") {
        Output::Dry(Recorder {
            echo: true,
            ..Default::default()
        })
    } else {
        Output::Real(RetryUinput::new())
    };
    let auto_update = cfg.auto_update;
    let mut d = Daemon {
        st: State::new(out, 1280, 800),
        watch,
        cfg,
        status: Status {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        },
        json_status: flags.get("status_file").map(PathBuf::from),
        updater: updater::Updater::spawn(auto_update),
        screens: screen::Detector::new(),
    };

    // Each iteration (re)applies the configuration; `serve` returns when the
    // config file changed.
    loop {
        let fixed = d.cfg.fixed_size();
        let (w, h) = match fixed {
            Some(f) => {
                d.status.screen_source = "set manually".into();
                f
            }
            None => match d.screens.detect() {
                Some(det) => {
                    d.status.screen_source = det.source;
                    (det.w, det.h)
                }
                None => {
                    d.status.screen_source = "not detected - assuming the Deck's screen".into();
                    (1280, 800)
                }
            },
        };
        d.st.deactivate();
        d.st.width = w;
        d.st.height = h;
        d.status.port = d.cfg.port;
        d.status.screen_auto = fixed.is_none();
        d.status.passphrase_set = !d.cfg.passphrase.is_empty();
        info!(
            "screen {w}x{h}{}",
            if fixed.is_some() {
                " (from config)"
            } else {
                ""
            }
        );
        let psk = (!d.cfg.passphrase.is_empty()).then(|| session::derive_psk(&d.cfg.passphrase));
        match UdpSocket::bind(("0.0.0.0", d.cfg.port)) {
            Ok(sock) => {
                sock.set_read_timeout(Some(Duration::from_millis(100))).ok();
                info!("listening on udp {}", d.cfg.port);
                d.status.net_error.clear();
                d.status.listening = true;
                serve(&sock, psk, &mut d);
            }
            Err(e) => {
                let msg = format!(
                    "cannot listen on UDP {}: {e} (is another glidekm client, e.g. the Decky plugin, running?)",
                    d.cfg.port
                );
                if d.status.net_error != msg {
                    warn!("{msg}");
                }
                d.status.net_error = msg;
                d.status.listening = false;
                // Keep publishing and retry; new settings apply right away.
                for _ in 0..10 {
                    publish(&mut d, None, "");
                    if let Some(c) = d.watch.poll() {
                        d.cfg = c;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
}

struct Peer {
    addr: SocketAddr,
    tr: Transport,
    last_rx: Instant,
    last_hello: Option<Instant>,
}

fn publish(d: &mut Daemon, peer: Option<&Peer>, rejected: &str) {
    d.st.sink.tick();
    // Settings app asked for an update check.
    if std::fs::remove_file(ipc::update_request_path()).is_ok() {
        d.updater.request_now();
    }
    // Install a staged update only while nobody is using the Deck.
    if !d.st.active {
        d.updater.install_if_staged();
    }
    {
        let u = d.updater.state.lock().unwrap();
        d.status.update_status = u.status.clone();
        d.status.update_available = u.available.clone();
        d.status.update_unsupported = u.unsupported;
    }
    let s = &mut d.status;
    s.updated = session::now_millis();
    s.server = peer.map(|p| p.addr.to_string()).unwrap_or_default();
    s.active = d.st.active;
    s.capture = d.st.capture;
    s.width = d.st.width;
    s.height = d.st.height;
    s.input_error = d.st.sink.error().to_string();
    s.last_rejected = rejected.to_string();
    settings::publish(s, d.json_status.as_ref());
}

fn serve(sock: &UdpSocket, psk: Option<[u8; 32]>, d: &mut Daemon) {
    let mut peer: Option<Peer> = None;
    let mut last_ts = 0u64;
    let mut buf = vec![0u8; 2048];
    let mut last_detect = Instant::now();
    let mut last_publish = Instant::now() - Duration::from_secs(10);
    let mut last_published = Status::default();
    let mut last_watch = Instant::now();
    let mut rejected = String::new();
    let autodetect = d.cfg.fixed_size().is_none();
    let mut last_announce: Option<Instant> = None;
    let _ = sock.set_broadcast(true);

    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let pkt = &buf[..n];
                match pkt.first() {
                    Some(&PKT_DISCOVER) if pkt.get(1..9) == Some(&session::DISCOVER_MAGIC[..]) => {
                        let _ = sock.send_to(&discovery::reply(&hostname()), from);
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
                                d.st.deactivate();
                                peer = Some(Peer {
                                    addr: from,
                                    tr,
                                    last_rx: Instant::now(),
                                    last_hello: None,
                                });
                                send_hello(sock, peer.as_mut().unwrap(), &d.st);
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
                                    d.st.handle(ev);
                                }
                                d.st.sink.flush();
                                if heartbeat
                                    && p.last_hello
                                        .is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
                                {
                                    send_hello(sock, p, &d.st);
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
                && d.st.active
            {
                warn!("server silent, releasing input");
                d.st.deactivate();
            }
            if p.last_rx.elapsed() > Duration::from_secs(10) {
                info!("server {} gone", p.addr);
                peer = None;
            }
        }

        // Not connected: tell PCs on the LAN where we are (they find us even
        // if their own broadcast probe is filtered).
        if peer.is_none()
            && psk.is_some()
            && last_announce.is_none_or(|t| t.elapsed() >= Duration::from_secs(3))
        {
            last_announce = Some(Instant::now());
            discovery::broadcast(
                sock,
                &discovery::announce(d.cfg.port, &hostname()),
                glidekm_core::DEFAULT_PORT,
            );
        }

        // Dock/undock changes the screen; re-check every few seconds.
        if autodetect && last_detect.elapsed() > Duration::from_secs(5) {
            last_detect = Instant::now();
            if let Some(det) = d.screens.detect() {
                let (w, h) = (det.w, det.h);
                d.status.screen_source = det.source.clone();
                if (w, h) != (d.st.width, d.st.height) {
                    info!("screen changed to {w}x{h} ({})", det.source);
                    d.st.width = w;
                    d.st.height = h;
                    if let Some(p) = peer.as_mut() {
                        send_hello(sock, p, &d.st);
                    }
                }
            }
        }

        // Settings changed on disk (glidekm Settings saved): apply.
        if last_watch.elapsed() >= Duration::from_millis(500) {
            last_watch = Instant::now();
            if let Some(c) = d.watch.poll() {
                d.updater
                    .auto
                    .store(c.auto_update, std::sync::atomic::Ordering::Relaxed);
                if c != d.cfg {
                    info!(
                        "settings changed in {} - applying",
                        d.watch.path().display()
                    );
                    d.cfg = c;
                    d.st.deactivate();
                    return;
                }
            }
        }

        // Publish state: on change (checked 10x/s) and as a 1 s heartbeat.
        if last_publish.elapsed() >= Duration::from_millis(100) {
            let mut probe = d.status.clone();
            probe.server = peer
                .as_ref()
                .map(|p| p.addr.to_string())
                .unwrap_or_default();
            probe.active = d.st.active;
            probe.capture = d.st.capture;
            probe.width = d.st.width;
            probe.height = d.st.height;
            probe.last_rejected = rejected.clone();
            probe.updated = last_published.updated;
            if probe != last_published || last_publish.elapsed() >= Duration::from_secs(1) {
                publish(d, peer.as_ref(), &rejected);
                last_published = d.status.clone();
                last_publish = Instant::now();
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
