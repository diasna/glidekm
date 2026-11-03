//! LAN discovery: broadcast a probe and collect glidekm clients that answer.

use crate::control::Control;
use glidekm_core::{debug, info, warn};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// Broadcast a discovery probe and collect answering clients.
pub fn scan(ctl: &Control) -> Vec<(String, String)> {
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
