//! Server networking (platform independent): handshake, encrypted sending,
//! heartbeats and client liveness. Input threads only push `Event`s into a
//! channel; this module never blocks them.

use glidekm_core::event::{encode_all, Event, KeyBitmap, KEY_BITMAP_LEN};
use glidekm_core::session::{self, Initiator, Transport, PKT_DATA, PKT_RESP};
use glidekm_core::{debug, discovery, info, warn};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// State shared between the input thread and the network threads.
pub struct Shared {
    /// Client answered recently (handshake done and Hello within timeout).
    pub alive: AtomicBool,
    /// Client screen size, `w << 16 | h`; 0 = unknown.
    pub client_size: AtomicU32,
    /// Input currently goes to the client.
    pub active: AtomicBool,
    pub capture: AtomicBool,
    /// Keys/buttons currently held on the client (for heartbeats).
    pub pressed: Mutex<KeyBitmap>,
    /// Where to connect and with which key; None = not configured yet.
    target: Mutex<Option<(String, [u8; 32])>>,
    /// Bumped whenever `target` changes, so the network threads reconnect.
    gen: AtomicU64,
    /// Resolved client address of the current attempt/session.
    pub peer: Mutex<Option<SocketAddr>>,
    /// Last connection problem, for status displays ("" = none).
    pub last_error: Mutex<String>,
    /// Hostname of the connected Deck (from discovery), if known.
    pub peer_name: Mutex<String>,
    /// No address configured: find the Deck on the LAN.
    pub auto: AtomicBool,
    /// Clipboard chunks arriving from the Deck, and a finished text waiting
    /// to be put on the clipboard.
    clip_asm: Mutex<glidekm_core::clip::Assembler>,
    pub clip_in: Mutex<Option<glidekm_core::clip::Content>>,
    /// Clipboard transfer in progress, for the UI.
    xfer: Mutex<Option<Xfer>>,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            alive: AtomicBool::new(false),
            client_size: AtomicU32::new(0),
            active: AtomicBool::new(false),
            capture: AtomicBool::new(false),
            pressed: Mutex::new([0; KEY_BITMAP_LEN]),
            target: Mutex::new(None),
            gen: AtomicU64::new(0),
            peer: Mutex::new(None),
            last_error: Mutex::new(String::new()),
            peer_name: Mutex::new(String::new()),
            auto: AtomicBool::new(false),
            clip_asm: Mutex::new(Default::default()),
            clip_in: Mutex::new(None),
            xfer: Mutex::new(None),
        })
    }

    /// Point the connection at a (new) client - an address, or "" to find
    /// it automatically. Takes effect immediately.
    pub fn set_target(&self, addr: &str, psk: [u8; 32]) {
        self.auto.store(is_auto(addr), Ordering::Relaxed);
        let mut t = self.target.lock().unwrap();
        let new = Some((addr.to_string(), psk));
        if *t != new {
            *t = new;
            self.gen.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub fn configured(&self) -> bool {
        self.target.lock().unwrap().is_some()
    }

    fn set_error(&self, e: &str) {
        *self.last_error.lock().unwrap() = e.to_string();
    }

    /// Report clipboard transfer progress (`kind` 0 text, 1 image).
    pub fn set_xfer(&self, sending: bool, kind: u8, done: usize, total: usize) {
        *self.xfer.lock().unwrap() = Some(Xfer {
            sending,
            kind,
            done,
            total,
            at: Instant::now(),
        });
    }

    /// Current clipboard transfer as (label, percent), if one is running or
    /// just finished. Small transfers are too quick to be worth showing.
    pub fn xfer_progress(&self) -> Option<(String, u32)> {
        let g = self.xfer.lock().unwrap();
        let x = g.as_ref()?;
        if x.total < 8 || x.at.elapsed() > Duration::from_secs(2) {
            return None;
        }
        let pct = (x.done * 100 / x.total).min(100) as u32;
        let what = if x.kind == 1 { "image" } else { "text" };
        let verb = if x.sending { "Sending" } else { "Receiving" };
        Some((format!("{verb} {what} {pct}%"), pct))
    }

    pub fn size(&self) -> Option<(u16, u16)> {
        let v = self.client_size.load(Ordering::Relaxed);
        (v != 0).then_some(((v >> 16) as u16, v as u16))
    }
}

struct Xfer {
    sending: bool,
    kind: u8,
    done: usize,
    total: usize,
    at: Instant,
}

pub fn is_auto(target: &str) -> bool {
    let t = target.trim();
    t.is_empty() || t.eq_ignore_ascii_case("auto")
}

#[derive(Default)]
struct Conn {
    /// The client of the current session.
    addr: Option<SocketAddr>,
    /// Handshakes in flight, one per candidate.
    pending: Vec<(SocketAddr, Initiator)>,
    tr: Option<Arc<Transport>>,
    last_hello: Option<Instant>,
    /// Auto mode: Decks seen on the LAN (address, hostname, last seen).
    candidates: Vec<(SocketAddr, String, Instant)>,
    /// When we started trying candidates without success.
    trying_since: Option<Instant>,
}

impl Conn {
    fn saw(&mut self, addr: SocketAddr, name: String) {
        match self.candidates.iter_mut().find(|c| c.0 == addr) {
            Some(c) => {
                c.1 = name;
                c.2 = Instant::now();
            }
            None => {
                info!(
                    "found glidekm on {} ({addr})",
                    if name.is_empty() { "a Deck" } else { &name }
                );
                self.candidates.push((addr, name, Instant::now()));
            }
        }
    }

    fn name_of(&self, addr: SocketAddr) -> String {
        self.candidates
            .iter()
            .find(|c| c.0 == addr)
            .map(|c| c.1.clone())
            .unwrap_or_default()
    }
}

pub fn spawn(shared: Arc<Shared>) -> std::io::Result<Sender<Event>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    let rsock = sock.try_clone()?;
    let conn = Arc::new(Mutex::new(Conn::default()));
    let (tx, rx) = mpsc::channel();

    // Decks announce themselves on the default port while unconnected.
    match UdpSocket::bind(("0.0.0.0", glidekm_core::DEFAULT_PORT)) {
        Ok(bsock) => {
            let c = conn.clone();
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("net-beacons".into())
                .spawn(move || beacon_loop(bsock, c, sh))?;
        }
        Err(e) => debug!("not listening for Deck announcements: {e}"),
    }

    let sh2 = shared.clone();
    let c = conn.clone();
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("net-rx".into())
        .spawn(move || recv_loop(rsock, c, sh))?;
    std::thread::Builder::new()
        .name("net-tx".into())
        .spawn(move || send_loop(sock, rx, conn, shared))?;
    crate::clipboard::spawn(sh2, tx.clone());
    Ok(tx)
}

fn resolve(target: &str) -> Option<SocketAddr> {
    let with_port = if target
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
        && !target.ends_with(']')
    {
        target.to_string()
    } else {
        format!("{target}:{}", glidekm_core::DEFAULT_PORT)
    };
    let addrs: Vec<SocketAddr> = with_port.to_socket_addrs().ok()?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
}

fn send_loop(sock: UdpSocket, rx: Receiver<Event>, conn: Arc<Mutex<Conn>>, shared: Arc<Shared>) {
    let hb = Duration::from_millis(glidekm_core::HEARTBEAT_MS);
    let timeout = Duration::from_millis(glidekm_core::CLIENT_TIMEOUT_MS);
    let mut last_hb = Instant::now() - hb;
    let mut last_init: Option<Instant> = None;
    let mut last_probe: Option<Instant> = None;
    let mut last_ts = 0u64;
    let mut warned_resolve = false;
    let mut batch = Vec::with_capacity(32);
    let mut my_gen = u64::MAX;

    loop {
        let first = match rx.recv_timeout(hb) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        batch.clear();
        batch.extend(first);
        // Coalesce whatever else is already queued into one datagram.
        while batch.len() < 64 {
            match rx.try_recv() {
                Ok(ev) => batch.push(ev),
                Err(_) => break,
            }
        }

        // Configuration changes: drop the session and reconnect.
        let gen = shared.gen.load(Ordering::SeqCst);
        if gen != my_gen {
            my_gen = gen;
            *conn.lock().unwrap() = Conn::default();
            *shared.peer.lock().unwrap() = None;
            shared.peer_name.lock().unwrap().clear();
            shared.client_size.store(0, Ordering::Relaxed);
            shared.set_error("");
            last_init = None;
            warned_resolve = false;
        }
        let Some((target, psk)) = shared.target.lock().unwrap().clone() else {
            shared.alive.store(false, Ordering::Relaxed);
            continue;
        };

        // Liveness.
        let (tr, addr) = {
            let c = conn.lock().unwrap();
            let alive = c.tr.is_some() && c.last_hello.is_some_and(|t| t.elapsed() < timeout);
            if shared.alive.swap(alive, Ordering::Relaxed) != alive {
                if alive {
                    info!("client online");
                } else {
                    warn!("client offline");
                }
            }
            (if alive { c.tr.clone() } else { None }, c.addr)
        };

        let Some(tr) = tr else {
            // (Re)connect: send INIT at most once per second.
            if last_init.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                last_init = Some(Instant::now());
                let auto = is_auto(&target);
                let targets: Vec<SocketAddr> = if auto {
                    // Ask the LAN every 3 s; Decks also announce themselves.
                    if last_probe.is_none_or(|t| t.elapsed() >= Duration::from_secs(3)) {
                        last_probe = Some(Instant::now());
                        discovery::broadcast(
                            &sock,
                            &discovery::probe(),
                            glidekm_core::DEFAULT_PORT,
                        );
                    }
                    let mut c = conn.lock().unwrap();
                    c.candidates
                        .retain(|x| x.2.elapsed() < Duration::from_secs(30));
                    let mut v: Vec<SocketAddr> = c.candidates.iter().map(|x| x.0).collect();
                    if let Some(a) = c.addr.filter(|a| !v.contains(a)) {
                        v.push(a); // the last Deck we talked to
                    }
                    v
                } else {
                    match resolve(&target) {
                        Some(a) => {
                            warned_resolve = false;
                            vec![a]
                        }
                        None => {
                            if !warned_resolve {
                                warned_resolve = true;
                                warn!("cannot resolve client address '{target}', retrying");
                                shared.set_error(&format!("cannot resolve '{target}'"));
                            }
                            Vec::new()
                        }
                    }
                };
                {
                    // Decks that never complete the handshake have a
                    // different passphrase (they stay silent by design).
                    let mut c = conn.lock().unwrap();
                    if targets.is_empty() {
                        c.trying_since = None;
                    } else {
                        let since = *c.trying_since.get_or_insert_with(Instant::now);
                        if auto && since.elapsed() > Duration::from_secs(5) {
                            let names: Vec<String> = c
                                .candidates
                                .iter()
                                .map(|x| {
                                    if x.1.is_empty() {
                                        x.0.ip().to_string()
                                    } else {
                                        format!("{} ({})", x.1, x.0.ip())
                                    }
                                })
                                .collect();
                            if !names.is_empty() {
                                shared.set_error(&format!(
                                    "Found {}, but the passphrase doesn't match",
                                    names.join(", ")
                                ));
                            }
                        }
                    }
                }
                let mut pending = Vec::new();
                for a in targets {
                    let ts = session::now_millis().max(last_ts + 1);
                    last_ts = ts;
                    if let Ok((ini, pkt)) = Initiator::start(&psk, ts) {
                        debug!("handshake -> {a}");
                        let _ = sock.send_to(&pkt, a);
                        pending.push((a, ini));
                    }
                }
                if !auto {
                    *shared.peer.lock().unwrap() = pending.first().map(|p| p.0);
                }
                conn.lock().unwrap().pending = pending;
            }
            continue;
        };
        let Some(addr) = addr else { continue };

        if last_hb.elapsed() >= hb {
            last_hb = Instant::now();
            batch.push(Event::Heartbeat {
                active: shared.active.load(Ordering::Relaxed),
                capture: shared.capture.load(Ordering::Relaxed),
                pressed: Box::new(*shared.pressed.lock().unwrap()),
            });
        }
        if batch.is_empty() {
            continue;
        }
        // Clipboard chunks travel alone so a datagram stays under the MTU.
        let (clips, rest): (Vec<Event>, Vec<Event>) = batch
            .drain(..)
            .partition(|e| matches!(e, Event::Clip { .. }));
        let groups = (!rest.is_empty())
            .then_some(rest)
            .into_iter()
            .chain(clips.into_iter().map(|c| vec![c]));
        for g in groups {
            match tr.seal(&encode_all(&g)) {
                Ok(pkt) => {
                    if let Err(e) = sock.send_to(&pkt, addr) {
                        debug!("send: {e}");
                    }
                }
                Err(e) => warn!("seal: {e}"),
            }
        }
    }
}

fn recv_loop(sock: UdpSocket, conn: Arc<Mutex<Conn>>, shared: Arc<Shared>) {
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                // Windows reports ICMP port-unreachable as a recv error; ignore.
                debug!("recv: {e}");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let pkt = &buf[..n];
        let mut c = conn.lock().unwrap();
        match pkt.first() {
            Some(&discovery::PKT_DISCOVER_REPLY) => {
                if shared.auto.load(Ordering::Relaxed) {
                    if let Some((a, name)) = discovery::parse_reply(pkt, from) {
                        c.saw(a, name);
                    }
                }
            }
            Some(&PKT_RESP) => {
                let Some(i) = c.pending.iter().position(|p| p.0 == from) else {
                    continue;
                };
                let (_, ini) = c.pending.swap_remove(i);
                match ini.finish(pkt) {
                    Ok(tr) => {
                        let name = c.name_of(from);
                        info!(
                            "session established with {}{from}",
                            if name.is_empty() {
                                String::new()
                            } else {
                                format!("{name} at ")
                            }
                        );
                        shared.set_error("");
                        c.addr = Some(from);
                        c.pending.clear();
                        c.trying_since = None;
                        c.tr = Some(Arc::new(tr));
                        c.last_hello = None;
                        *shared.peer.lock().unwrap() = Some(from);
                        *shared.peer_name.lock().unwrap() = name;
                    }
                    Err(e) => {
                        let name = c.name_of(from);
                        let who = if name.is_empty() {
                            from.ip().to_string()
                        } else {
                            format!("{name} ({})", from.ip())
                        };
                        warn!("handshake with {who} failed ({e}) - passphrase mismatch?");
                        shared.set_error(&format!("Found {who}, but its passphrase is different"));
                    }
                }
            }
            Some(&PKT_DATA) => {
                if c.addr != Some(from) {
                    continue;
                }
                let Some(tr) = c.tr.clone() else { continue };
                let Ok(plain) = tr.open(pkt) else { continue };
                for ev in Event::decode_all(&plain) {
                    if let Event::Clip {
                        id,
                        kind,
                        idx,
                        count,
                        data,
                    } = &ev
                    {
                        let (done, (got, total)) = {
                            let mut a = shared.clip_asm.lock().unwrap();
                            let d = a.push(*id, *kind, *idx, *count, data);
                            (d, a.progress())
                        };
                        if done.is_some() {
                            shared.set_xfer(false, *kind, *count as usize, *count as usize);
                        } else {
                            shared.set_xfer(false, *kind, got, total);
                        }
                        if done.is_some() {
                            *shared.clip_in.lock().unwrap() = done;
                        }
                    }
                    if let Event::Hello { width, height } = ev {
                        let v = (width as u32) << 16 | height as u32;
                        if shared.client_size.swap(v, Ordering::Relaxed) != v {
                            info!("client screen {width}x{height}");
                        }
                        c.last_hello = Some(Instant::now());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Collects Decks that announce themselves (auto mode only).
fn beacon_loop(sock: UdpSocket, conn: Arc<Mutex<Conn>>, shared: Arc<Shared>) {
    let mut buf = [0u8; 256];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        if !shared.auto.load(Ordering::Relaxed) {
            continue;
        }
        if let Some((a, name)) = discovery::parse_announce(&buf[..n], from) {
            conn.lock().unwrap().saw(a, name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolve_adds_default_port() {
        assert_eq!(
            resolve("127.0.0.1").unwrap().port(),
            glidekm_core::DEFAULT_PORT
        );
        assert_eq!(resolve("127.0.0.1:9").unwrap().port(), 9);
    }
}
