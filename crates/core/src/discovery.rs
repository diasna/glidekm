//! LAN discovery (unauthenticated - it only finds candidates; a Deck gets
//! input only after the passphrase-keyed handshake succeeds).
//!
//! - PC -> broadcast: `[4] "glidekm?"`              (probe)
//! - Deck -> PC:      `[5] "glidekm!" hostname`     (probe reply)
//! - Deck -> broadcast, while not connected (announce):
//!   `[6] "glidekm#" port(u16 LE) hostname`

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

pub use crate::session::{DISCOVER_MAGIC, DISCOVER_REPLY_MAGIC, PKT_DISCOVER, PKT_DISCOVER_REPLY};
pub const PKT_ANNOUNCE: u8 = 6;
pub const ANNOUNCE_MAGIC: &[u8; 8] = b"glidekm#";

pub fn probe() -> Vec<u8> {
    let mut p = vec![PKT_DISCOVER];
    p.extend_from_slice(DISCOVER_MAGIC);
    p
}

pub fn reply(hostname: &str) -> Vec<u8> {
    let mut r = vec![PKT_DISCOVER_REPLY];
    r.extend_from_slice(DISCOVER_REPLY_MAGIC);
    r.extend_from_slice(clean(hostname).as_bytes());
    r
}

pub fn announce(port: u16, hostname: &str) -> Vec<u8> {
    let mut a = vec![PKT_ANNOUNCE];
    a.extend_from_slice(ANNOUNCE_MAGIC);
    a.extend_from_slice(&port.to_le_bytes());
    a.extend_from_slice(clean(hostname).as_bytes());
    a
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// A probe reply from `from`: (client address, hostname).
pub fn parse_reply(pkt: &[u8], from: SocketAddr) -> Option<(SocketAddr, String)> {
    if pkt.first() != Some(&PKT_DISCOVER_REPLY) || pkt.get(1..9) != Some(&DISCOVER_REPLY_MAGIC[..])
    {
        return None;
    }
    Some((from, clean(&String::from_utf8_lossy(&pkt[9..]))))
}

/// An announce from `from`: (client address = sender IP + announced port, hostname).
pub fn parse_announce(pkt: &[u8], from: SocketAddr) -> Option<(SocketAddr, String)> {
    if pkt.first() != Some(&PKT_ANNOUNCE) || pkt.get(1..9) != Some(&ANNOUNCE_MAGIC[..]) {
        return None;
    }
    let port = u16::from_le_bytes([*pkt.get(9)?, *pkt.get(10)?]);
    if port == 0 {
        return None;
    }
    let name = clean(&String::from_utf8_lossy(&pkt[11..]));
    Some((SocketAddr::new(from.ip(), port), name))
}

/// Broadcast destinations for `port`: the limited broadcast plus the /24
/// directed broadcast of the primary interface (some networks only pass that).
pub fn targets(port: u16) -> Vec<SocketAddr> {
    let mut v = vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, port))];
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("192.0.2.1:9").is_ok() {
            if let Ok(SocketAddr::V4(a)) = s.local_addr() {
                let o = a.ip().octets();
                if !a.ip().is_unspecified() && !a.ip().is_loopback() {
                    v.push(SocketAddr::V4(SocketAddrV4::new(
                        Ipv4Addr::new(o[0], o[1], o[2], 255),
                        port,
                    )));
                }
            }
        }
    }
    v
}

/// Send `pkt` to every broadcast target (socket must allow broadcast).
pub fn broadcast(sock: &UdpSocket, pkt: &[u8], port: u16) {
    for t in targets(port) {
        if let Err(e) = sock.send_to(pkt, t) {
            crate::debug!("broadcast to {t}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let from: SocketAddr = "192.168.1.50:4242".parse().unwrap();
        assert_eq!(
            parse_reply(&reply("steamdeck"), from),
            Some((from, "steamdeck".into()))
        );
        let a = announce(5000, "deck\n");
        let got = parse_announce(&a, "192.168.1.50:41000".parse().unwrap()).unwrap();
        assert_eq!(got, ("192.168.1.50:5000".parse().unwrap(), "deck".into()));
        assert!(parse_announce(&probe(), from).is_none());
        assert!(parse_reply(&announce(1, "x"), from).is_none());
        assert!(parse_announce(&[PKT_ANNOUNCE], from).is_none());
        assert_eq!(
            targets(4242)[0],
            "255.255.255.255:4242".parse::<SocketAddr>().unwrap()
        );
    }
}
