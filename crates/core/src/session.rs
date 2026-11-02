//! Encrypted UDP session: Noise NNpsk0 handshake (pre-shared passphrase),
//! then stateless transport with an explicit 64-bit nonce per packet and a
//! sliding replay window.
//!
//! Packet layout:
//!   INIT:  [1][noise msg 1]          payload = u64 LE timestamp (anti-replay)
//!   RESP:  [2][noise msg 2]
//!   DATA:  [3][nonce u64 LE][ciphertext]

use snow::params::HashChoice;
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::{Builder, HandshakeState, StatelessTransportState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const PKT_INIT: u8 = 1;
pub const PKT_RESP: u8 = 2;
pub const PKT_DATA: u8 = 3;
/// Unauthenticated LAN discovery: `[4]"glidekm?"` -> `[5]"glidekm!"<hostname>`.
pub const PKT_DISCOVER: u8 = 4;
pub const PKT_DISCOVER_REPLY: u8 = 5;
pub const DISCOVER_MAGIC: &[u8; 8] = b"glidekm?";
pub const DISCOVER_REPLY_MAGIC: &[u8; 8] = b"glidekm!";
pub const MAX_PACKET: usize = 1400;

const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"glidekm/1";

#[derive(Debug)]
pub enum Error {
    Noise(snow::Error),
    Malformed,
    Replay,
}

impl From<snow::Error> for Error {
    fn from(e: snow::Error) -> Self {
        Error::Noise(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Noise(e) => write!(f, "crypto: {e}"),
            Error::Malformed => write!(f, "malformed packet"),
            Error::Replay => write!(f, "replayed/stale packet"),
        }
    }
}

/// Derive the 32-byte PSK from a passphrase (BLAKE2s, domain separated).
pub fn derive_psk(passphrase: &str) -> [u8; 32] {
    let mut h = DefaultResolver
        .resolve_hash(&HashChoice::Blake2s)
        .expect("blake2s");
    h.input(b"glidekm-psk-v1\0");
    h.input(passphrase.as_bytes());
    let mut out = [0u8; 32];
    h.result(&mut out);
    out
}

fn params() -> snow::params::NoiseParams {
    PARAMS.parse().expect("valid noise params")
}

/// Sliding replay window over the last 128 nonces.
#[derive(Default)]
struct ReplayWindow {
    top: u64,
    bits: u128,
    any: bool,
}

impl ReplayWindow {
    fn check_and_set(&mut self, n: u64) -> bool {
        if !self.any {
            self.any = true;
            self.top = n;
            self.bits = 1;
            return true;
        }
        if n > self.top {
            let shift = n - self.top;
            self.bits = if shift >= 128 { 0 } else { self.bits << shift };
            self.bits |= 1;
            self.top = n;
            true
        } else {
            let off = self.top - n;
            if off >= 128 || self.bits & (1u128 << off) != 0 {
                false
            } else {
                self.bits |= 1u128 << off;
                true
            }
        }
    }
}

pub struct Transport {
    st: StatelessTransportState,
    send_nonce: AtomicU64,
    replay: Mutex<ReplayWindow>,
}

impl Transport {
    fn new(st: StatelessTransportState) -> Self {
        Transport {
            st,
            send_nonce: AtomicU64::new(0),
            replay: Mutex::new(ReplayWindow::default()),
        }
    }

    /// Encrypt `plain` into a DATA packet.
    pub fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, Error> {
        let nonce = self.send_nonce.fetch_add(1, Ordering::Relaxed);
        let mut out = vec![0u8; 9 + plain.len() + 16];
        out[0] = PKT_DATA;
        out[1..9].copy_from_slice(&nonce.to_le_bytes());
        let n = self.st.write_message(nonce, plain, &mut out[9..])?;
        out.truncate(9 + n);
        Ok(out)
    }

    /// Decrypt a DATA packet (including its type byte).
    pub fn open(&self, pkt: &[u8]) -> Result<Vec<u8>, Error> {
        if pkt.len() < 9 + 16 || pkt[0] != PKT_DATA {
            return Err(Error::Malformed);
        }
        let nonce = u64::from_le_bytes(pkt[1..9].try_into().unwrap());
        let mut out = vec![0u8; pkt.len()];
        let n = self.st.read_message(nonce, &pkt[9..], &mut out)?;
        // Only record the nonce once the packet authenticated.
        if !self.replay.lock().unwrap().check_and_set(nonce) {
            return Err(Error::Replay);
        }
        out.truncate(n);
        Ok(out)
    }
}

/// Server side: starts the handshake.
pub struct Initiator {
    hs: HandshakeState,
}

impl Initiator {
    /// Returns the initiator and the INIT packet to send. `timestamp` must
    /// increase between attempts (milliseconds since epoch works).
    pub fn start(psk: &[u8; 32], timestamp: u64) -> Result<(Initiator, Vec<u8>), Error> {
        let mut hs = Builder::new(params())
            .prologue(PROLOGUE)?
            .psk(0, psk)?
            .build_initiator()?;
        let mut buf = vec![0u8; 256];
        buf[0] = PKT_INIT;
        let n = hs.write_message(&timestamp.to_le_bytes(), &mut buf[1..])?;
        buf.truncate(1 + n);
        Ok((Initiator { hs }, buf))
    }

    pub fn finish(mut self, resp: &[u8]) -> Result<Transport, Error> {
        if resp.first() != Some(&PKT_RESP) {
            return Err(Error::Malformed);
        }
        let mut payload = [0u8; 64];
        self.hs.read_message(&resp[1..], &mut payload)?;
        Ok(Transport::new(self.hs.into_stateless_transport_mode()?))
    }
}

/// Client side: answer an INIT packet. `last_ts` is the highest accepted
/// timestamp, which rejects replayed INIT packets.
pub fn respond(
    psk: &[u8; 32],
    init: &[u8],
    last_ts: &mut u64,
) -> Result<(Transport, Vec<u8>), Error> {
    if init.first() != Some(&PKT_INIT) {
        return Err(Error::Malformed);
    }
    let mut hs = Builder::new(params())
        .prologue(PROLOGUE)?
        .psk(0, psk)?
        .build_responder()?;
    let mut payload = [0u8; 64];
    let n = hs.read_message(&init[1..], &mut payload)?;
    if n != 8 {
        return Err(Error::Malformed);
    }
    let ts = u64::from_le_bytes(payload[..8].try_into().unwrap());
    if ts <= *last_ts {
        return Err(Error::Replay);
    }
    let mut buf = vec![0u8; 256];
    buf[0] = PKT_RESP;
    let m = hs.write_message(&[], &mut buf[1..])?;
    buf.truncate(1 + m);
    *last_ts = ts;
    Ok((Transport::new(hs.into_stateless_transport_mode()?), buf))
}

/// 32 bytes from the OS random number generator.
pub fn random_bytes() -> [u8; 32] {
    let kp = Builder::new(params()).generate_keypair().expect("os rng");
    let mut out = [0u8; 32];
    out.copy_from_slice(&kp.private[..32]);
    out
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_and_transport() {
        let psk = derive_psk("hunter2");
        let mut last = 0;
        let (ini, init_pkt) = Initiator::start(&psk, 1000).unwrap();
        let (resp_t, resp_pkt) = respond(&psk, &init_pkt, &mut last).unwrap();
        let ini_t = ini.finish(&resp_pkt).unwrap();

        let p = ini_t.seal(b"hello").unwrap();
        assert_eq!(resp_t.open(&p).unwrap(), b"hello");
        // replay rejected
        assert!(matches!(resp_t.open(&p), Err(Error::Replay)));
        // reverse direction
        let q = resp_t.seal(b"pong").unwrap();
        assert_eq!(ini_t.open(&q).unwrap(), b"pong");
        // tampering rejected
        let mut bad = ini_t.seal(b"x").unwrap();
        *bad.last_mut().unwrap() ^= 1;
        assert!(resp_t.open(&bad).is_err());
        // out of order within window is fine
        let a = ini_t.seal(b"a").unwrap();
        let b = ini_t.seal(b"b").unwrap();
        assert_eq!(resp_t.open(&b).unwrap(), b"b");
        assert_eq!(resp_t.open(&a).unwrap(), b"a");
    }

    #[test]
    fn wrong_psk_and_replayed_init() {
        let mut last = 0;
        let (_i, init_pkt) = Initiator::start(&derive_psk("a"), 5).unwrap();
        assert!(respond(&derive_psk("b"), &init_pkt, &mut last).is_err());
        assert!(respond(&derive_psk("a"), &init_pkt, &mut last).is_ok());
        assert!(matches!(
            respond(&derive_psk("a"), &init_pkt, &mut last),
            Err(Error::Replay)
        ));
    }
}
