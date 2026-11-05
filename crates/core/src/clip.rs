//! Clipboard sharing (text and PNG images): the content is split into
//! `Event::Clip` chunks that each fit one datagram, and reassembled on the
//! other side.

use crate::event::Event;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Payload bytes per chunk (leaves room for the header, AEAD tag and batching).
pub const CHUNK: usize = 1000;
/// Larger text clipboards are not shared.
pub const MAX_TEXT: usize = 256 * 1024;
/// Larger images are not shared.
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;
const MAX_CHUNKS: usize = MAX_IMAGE / CHUNK + 2;

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// What is on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// UTF-8 text with `\n` line endings.
    Text(String),
    /// A PNG file.
    Png(Vec<u8>),
}

impl Content {
    fn kind(&self) -> u8 {
        match self {
            Content::Text(_) => 0,
            Content::Png(_) => 1,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Content::Text(s) => s.as_bytes(),
            Content::Png(b) => b,
        }
    }

    pub fn hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.kind().hash(&mut h);
        self.bytes().hash(&mut h);
        h.finish()
    }

    fn from_parts(kind: u8, bytes: Vec<u8>) -> Option<Content> {
        match kind {
            0 if bytes.len() <= MAX_TEXT => String::from_utf8(bytes)
                .ok()
                .filter(|s| !s.is_empty())
                .map(Content::Text),
            1 if bytes.len() <= MAX_IMAGE && bytes.starts_with(PNG_MAGIC) => {
                Some(Content::Png(bytes))
            }
            _ => None,
        }
    }
}

/// Line endings on the wire are always `\n`.
pub fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Split content into chunk events, or None if empty/too large.
pub fn split(id: u32, c: &Content) -> Option<Vec<Event>> {
    let b = c.bytes();
    let max = match c {
        Content::Text(_) => MAX_TEXT,
        Content::Png(_) => MAX_IMAGE,
    };
    if b.is_empty() || b.len() > max {
        return None;
    }
    let count = b.len().div_ceil(CHUNK) as u16;
    Some(
        b.chunks(CHUNK)
            .enumerate()
            .map(|(i, d)| Event::Clip {
                id,
                kind: c.kind(),
                idx: i as u16,
                count,
                data: d.to_vec(),
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
    /// (chunks received, chunks expected) of the transfer in progress.
    pub fn progress(&self) -> (usize, usize) {
        (
            self.parts.iter().filter(|p| p.is_some()).count(),
            self.parts.len(),
        )
    }

    pub fn push(
        &mut self,
        id: u32,
        kind: u8,
        idx: u16,
        count: u16,
        data: &[u8],
    ) -> Option<Content> {
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
        Content::from_parts(kind, bytes)
    }
}

/// Detects local clipboard changes, ignoring what we set ourselves.
#[derive(Default)]
pub struct Watcher {
    last: Option<u64>,
}

impl Watcher {
    /// Record content we just wrote to the clipboard (so it isn't sent back).
    pub fn set_remote(&mut self, c: &Content) {
        self.last = Some(c.hash());
    }

    /// Feed the current clipboard. True when it changed since the last call
    /// and should be sent; `send` false only records it (not connected), and
    /// the very first look is only recorded.
    pub fn changed(&mut self, current: &Content, send: bool) -> bool {
        let h = current.hash();
        let first = self.last.is_none();
        let differs = self.last != Some(h);
        self.last = Some(h);
        differs && send && !first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(c: &Content) -> Option<Content> {
        let mut a = Assembler::default();
        let mut out = None;
        for e in split(7, c).unwrap().iter().rev() {
            if let Event::Clip {
                id,
                kind,
                idx,
                count,
                data,
            } = e
            {
                out = a.push(*id, *kind, *idx, *count, data);
            }
        }
        out
    }

    #[test]
    fn roundtrip_out_of_order() {
        let text = Content::Text("héllo wörld ".repeat(500));
        assert_eq!(feed(&text).unwrap(), text);
        let mut png = PNG_MAGIC.to_vec();
        png.extend((0..50_000u32).map(|i| i as u8));
        let img = Content::Png(png);
        assert_eq!(feed(&img).unwrap(), img);
    }

    #[test]
    fn rejects_bad_png() {
        assert!(Content::from_parts(1, b"not a png".to_vec()).is_none());
    }

    #[test]
    fn watcher_ignores_remote_and_first() {
        let t = |s: &str| Content::Text(s.into());
        let mut w = Watcher::default();
        assert!(!w.changed(&t("a"), true)); // first look
        assert!(w.changed(&t("b"), true));
        assert!(!w.changed(&t("b"), true));
        w.set_remote(&t("c"));
        assert!(!w.changed(&t("c"), true));
        assert!(!w.changed(&t("d"), false)); // offline: recorded only
        assert!(!w.changed(&t("d"), true));
    }
}
