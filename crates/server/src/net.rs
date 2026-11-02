//! Server networking (platform independent): handshake, encrypted sending,
//! heartbeats and client liveness. Input threads only push `Event`s into a
//! channel; this module never blocks them.

use glidekm_core::event::{encode_all, Event, KeyBitmap, KEY_BITMAP_LEN};
use glidekm_core::session::{self, Initiator, Transport, PKT_DATA, PKT_RESP};
use glidekm_core::{debug, info, warn};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            alive: AtomicBool::new(false),
            client_size: AtomicU32::new(0),
            active: AtomicBool::new(false),
            capture: AtomicBool::new(false),
            pressed: Mutex::new([0; KEY_BITMAP_LEN]),
        })
    }

    pub fn size(&self) -> Option<(u16, u16)> {
        let v = self.client_size.load(Ordering::Relaxed);
        (v != 0).then_some(((v >> 16) as u16, v as u16))
    }
}

struct Conn {
    addr: Option<SocketAddr>,
    pending: Option<Initiator>,
    tr: Option<Arc<Transport>>,
    last_hello: Option<Instant>,
}

pub fn spawn(target: String, psk: [u8; 32], shared: Arc<Shared>) -> std::io::Result<Sender<Event>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let rsock = sock.try_clone()?;
    let conn = Arc::new(Mutex::new(Conn {
        addr: None,
        pending: None,
        tr: None,
        last_hello: None,
    }));
    let (tx, rx) = mpsc::channel();

    let c = conn.clone();
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("net-rx".into())
        .spawn(move || recv_loop(rsock, c, sh))?;
    std::thread::Builder::new()
        .name("net-tx".into())
        .spawn(move || send_loop(sock, target, psk, rx, conn, shared))?;
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

fn send_loop(
    sock: UdpSocket,
    target: String,
    psk: [u8; 32],
    rx: Receiver<Event>,
    conn: Arc<Mutex<Conn>>,
    shared: Arc<Shared>,
) {
    let hb = Duration::from_millis(glidekm_core::HEARTBEAT_MS);
    let timeout = Duration::from_millis(glidekm_core::CLIENT_TIMEOUT_MS);
    let mut last_hb = Instant::now() - hb;
    let mut last_init: Option<Instant> = None;
    let mut last_ts = 0u64;
    let mut warned_resolve = false;
    let mut batch = Vec::with_capacity(32);

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
                match resolve(&target) {
                    Some(a) => {
                        warned_resolve = false;
                        let ts = session::now_millis().max(last_ts + 1);
                        last_ts = ts;
                        if let Ok((ini, pkt)) = Initiator::start(&psk, ts) {
                            {
                                let mut c = conn.lock().unwrap();
                                c.pending = Some(ini);
                                c.addr = Some(a);
                            }
                            debug!("handshake -> {a}");
                            let _ = sock.send_to(&pkt, a);
                        }
                    }
                    None if !warned_resolve => {
                        warned_resolve = true;
                        warn!("cannot resolve client address '{target}', retrying");
                    }
                    None => {}
                }
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
        let plain = encode_all(&batch);
        match tr.seal(&plain) {
            Ok(pkt) => {
                if let Err(e) = sock.send_to(&pkt, addr) {
                    debug!("send: {e}");
                }
            }
            Err(e) => warn!("seal: {e}"),
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
        if c.addr != Some(from) {
            continue;
        }
        match pkt.first() {
            Some(&PKT_RESP) => {
                if let Some(ini) = c.pending.take() {
                    match ini.finish(pkt) {
                        Ok(tr) => {
                            info!("session established with {from}");
                            c.tr = Some(Arc::new(tr));
                            c.last_hello = None;
                        }
                        Err(e) => warn!("handshake failed ({e}) - passphrase mismatch?"),
                    }
                }
            }
            Some(&PKT_DATA) => {
                let Some(tr) = c.tr.clone() else { continue };
                let Ok(plain) = tr.open(pkt) else { continue };
                for ev in Event::decode_all(&plain) {
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
