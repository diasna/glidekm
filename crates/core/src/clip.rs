//! Clipboard sharing (text only): a text is split into `Event::Clip` chunks
//! that each fit one datagram, and reassembled on the other side.

use crate::event::Event;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Payload bytes per chunk (leaves room for the header, AEAD tag and batching).
pub const CHUNK: usize = 1000;
/// Larger clipboards are not shared.
pub const MAX_BYTES: usize = 256 * 1024;
const MAX_CHUNKS: usize = MAX_BYTES / CHUNK + 2;

/// Line endings on the wire are always `\n`.
pub fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n")
}

pub fn hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Split `text` into chunk events, or None if empty/too large.
pub fn split(id: u32, text: &str) -> Option<Vec<Event>> {
    let b = text.as_bytes();
    if b.is_empty() || b.len() > MAX_BYTES {
        return None;
    }
    let count = b.len().div_ceil(CHUNK) as u16;
    Some(
        b.chunks(CHUNK)
            .enumerate()
            .map(|(i, c)| Event::Clip {
                id,
                idx: i as u16,
                count,
                data: c.to_vec(),
            })
            .collect(),
    )
}

/// Reassembles chunks of one clipboard transfer; a newer id replaces a
/// transfer that never completed (lost datagram).
#[derive(Default)]
pub struct Assembler {
    id: u32,
    parts: Vec<Option<Vec<u8>>>,
}

impl Assembler {
    pub fn push(&mut self, id: u32, idx: u16, count: u16, data: &[u8]) -> Option<String> {
        let (idx, count) = (idx as usize, count as usize);
        if count == 0 || count > MAX_CHUNKS || idx >= count {
            return None;
        }
        if self.id != id || self.parts.len() != count {
            self.id = id;
            self.parts = vec![None; count];
        }
        self.parts[idx] = Some(data.to_vec());
        if self.parts.iter().any(|p| p.is_none()) {
            return None;
        }
        let bytes: Vec<u8> = self.parts.drain(..).flatten().flatten().collect();
        self.parts.clear();
        String::from_utf8(bytes).ok().filter(|s| !s.is_empty())
    }
}

/// Detects local clipboard changes, ignoring what we set ourselves.
#[derive(Default)]
pub struct Watcher {
    last: Option<u64>,
}

impl Watcher {
    /// Record text we just wrote to the clipboard (so it isn't sent back).
    pub fn set_remote(&mut self, text: &str) {
        self.last = Some(hash(text));
    }

    /// Feed the current clipboard text. Returns it when it changed since the
    /// last call; `send` false only records it (not connected / first look).
    pub fn changed(&mut self, current: &str, send: bool) -> bool {
        let h = hash(current);
        let first = self.last.is_none();
        let differs = self.last != Some(h);
        self.last = Some(h);
        differs && send && !first && !current.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_out_of_order() {
        let text: String = "héllo wörld ".repeat(500);
        let evs = split(7, &text).unwrap();
        assert!(evs.len() > 1);
        let mut a = Assembler::default();
        let mut out = None;
        for e in evs.iter().rev() {
            if let Event::Clip { id, idx, count, data } = e {
                out = a.push(*id, *idx, *count, data);
            }
        }
        assert_eq!(out.unwrap(), text);
    }

    #[test]
    fn watcher_ignores_remote_and_first() {
        let mut w = Watcher::default();
        assert!(!w.changed("a", true)); // first look
        assert!(w.changed("b", true));
        assert!(!w.changed("b", true));
        w.set_remote("c");
        assert!(!w.changed("c", true));
        assert!(!w.changed("d", false)); // offline: recorded only
        assert!(!w.changed("d", true));
    }
}
