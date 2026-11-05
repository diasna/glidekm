//! Clipboard sharing (text, PNG images, files): the content is split into
//! `Event::Clip` chunks that each fit one datagram, and reassembled on the
//! other side. The receiver asks for missing chunks (`Event::ClipNack`), the
//! sender keeps the last transfer around to answer.

use crate::event::Event;
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Payload bytes per chunk (leaves room for the header, AEAD tag and batching).
pub const CHUNK: usize = 1000;
/// Larger text clipboards are not shared.
pub const MAX_TEXT: usize = 256 * 1024;
/// Larger images are not shared.
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;
/// Total size limit for copied files.
pub const MAX_FILES: usize = 32 * 1024 * 1024;
const MAX_CHUNKS: usize = MAX_FILES / CHUNK + 2048;
/// Chunk numbers asked for per repair request.
const NACK_MAX: usize = 300;
/// An unfinished incoming transfer is dropped after this long without data.
const GIVE_UP: Duration = Duration::from_secs(15);
/// How long the sender keeps a transfer for repair requests.
const KEEP: Duration = Duration::from_secs(120);

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileItem {
    pub name: String,
    pub data: Vec<u8>,
}

/// What is on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// UTF-8 text with `\n` line endings.
    Text(String),
    /// A PNG file.
    Png(Vec<u8>),
    /// Copied files (no folders).
    Files(Vec<FileItem>),
}

impl Content {
    fn kind(&self) -> u8 {
        match self {
            Content::Text(_) => 0,
            Content::Png(_) => 1,
            Content::Files(_) => 2,
        }
    }

    fn limit(&self) -> usize {
        match self {
            Content::Text(_) => MAX_TEXT,
            Content::Png(_) => MAX_IMAGE,
            Content::Files(_) => MAX_FILES,
        }
    }

    /// The bytes that travel.
    pub fn payload(&self) -> Cow<'_, [u8]> {
        match self {
            Content::Text(s) => Cow::Borrowed(s.as_bytes()),
            Content::Png(b) => Cow::Borrowed(b),
            Content::Files(f) => {
                let mut out = Vec::new();
                out.extend_from_slice(&(f.len() as u16).to_le_bytes());
                for i in f {
                    let n = i.name.as_bytes();
                    out.extend_from_slice(&(n.len() as u16).to_le_bytes());
                    out.extend_from_slice(n);
                    out.extend_from_slice(&(i.data.len() as u64).to_le_bytes());
                    out.extend_from_slice(&i.data);
                }
                Cow::Owned(out)
            }
        }
    }

    /// Size in bytes, for logs and the "is it worth showing" decision.
    pub fn size(&self) -> usize {
        match self {
            Content::Text(s) => s.len(),
            Content::Png(b) => b.len(),
            Content::Files(f) => f.iter().map(|i| i.data.len()).sum(),
        }
    }

    pub fn hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.kind().hash(&mut h);
        match self {
            Content::Text(s) => s.hash(&mut h),
            Content::Png(b) => b.hash(&mut h),
            Content::Files(f) => f.hash(&mut h),
        }
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
            2 if bytes.len() <= MAX_FILES + 64 * 1024 => decode_files(&bytes).map(Content::Files),
            _ => None,
        }
    }
}

fn decode_files(b: &[u8]) -> Option<Vec<FileItem>> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(at..at.checked_add(n)?)?;
        at += n;
        Some(s)
    };
    let n = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    let mut out = Vec::new();
    for _ in 0..n {
        let nl = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
        let name = sanitize_name(&String::from_utf8_lossy(take(nl)?));
        let len = u64::from_le_bytes(take(8)?.try_into().ok()?) as usize;
        out.push(FileItem {
            name,
            data: take(len)?.to_vec(),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// Line endings on the wire are always `\n`.
pub fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// A file name that is safe to create: no directories, no characters either
/// OS rejects, never empty.
pub fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c))
        .take(120)
        .collect();
    let cleaned = cleaned
        .trim_matches(|c: char| c == ' ' || c == '.')
        .to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

/// Write received files into `dir` (created); returns their paths.
pub fn save_files(dir: &Path, files: &[FileItem]) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for f in files {
        let mut p = dir.join(sanitize_name(&f.name));
        let mut n = 1;
        while paths.contains(&p) {
            n += 1;
            let (stem, ext) = match f.name.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                _ => (f.name.clone(), String::new()),
            };
            p = dir.join(sanitize_name(&format!("{stem} ({n}){ext}")));
        }
        std::fs::write(&p, &f.data)?;
        paths.push(p);
    }
    Ok(paths)
}

/// Read copied files. Folders are skipped; None if nothing usable or the
/// total is over the limit.
pub fn load_files(paths: &[PathBuf]) -> Option<Vec<FileItem>> {
    let mut total = 0u64;
    let mut list = Vec::new();
    for p in paths {
        let Ok(m) = std::fs::metadata(p) else {
            continue;
        };
        if !m.is_file() {
            continue;
        }
        total += m.len();
        if total > MAX_FILES as u64 {
            return None;
        }
        list.push(p);
    }
    let mut out = Vec::new();
    for p in list {
        let data = std::fs::read(p).ok()?;
        let name = sanitize_name(&p.file_name()?.to_string_lossy());
        out.push(FileItem { name, data });
    }
    (!out.is_empty() && out.len() <= u16::MAX as usize).then_some(out)
}

/// Split content into chunk events, or None if empty/too large.
pub fn split(id: u32, c: &Content) -> Option<Vec<Event>> {
    let b = c.payload();
    if b.is_empty() || b.len() > c.limit() + 64 * 1024 {
        return None;
    }
    let count = b.len().div_ceil(CHUNK);
    if count > MAX_CHUNKS {
        return None;
    }
    Some(
        b.chunks(CHUNK)
            .enumerate()
            .map(|(i, d)| Event::Clip {
                id,
                kind: c.kind(),
                idx: i as u16,
                count: count as u16,
                data: d.to_vec(),
            })
            .collect(),
    )
}

/// The last outgoing transfer, kept to answer repair requests.
pub struct Outbox {
    id: u32,
    chunks: Vec<Event>,
    at: Instant,
}

impl Outbox {
    pub fn new(id: u32, chunks: Vec<Event>) -> Outbox {
        Outbox {
            id,
            chunks,
            at: Instant::now(),
        }
    }

    /// Chunks to resend for a repair request (empty if it isn't ours / too old).
    pub fn resend(&self, id: u32, missing: &[u16]) -> Vec<Event> {
        if id != self.id || self.at.elapsed() > KEEP {
            return Vec::new();
        }
        missing
            .iter()
            .filter_map(|&i| self.chunks.get(i as usize).cloned())
            .collect()
    }
}

/// Reassembles chunks of one clipboard transfer; a newer id replaces a
/// transfer that never completed.
#[derive(Default)]
pub struct Assembler {
    id: u32,
    parts: Vec<Option<Vec<u8>>>,
    last_rx: Option<Instant>,
    last_nack: Option<Instant>,
    done_id: Option<u32>,
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
        if count == 0 || count > MAX_CHUNKS || idx >= count || self.done_id == Some(id) {
            return None;
        }
        if self.id != id || self.parts.len() != count {
            self.id = id;
            self.parts = vec![None; count];
            self.last_nack = None;
        }
        self.last_rx = Some(Instant::now());
        self.parts[idx] = Some(data.to_vec());
        if self.parts.iter().any(|p| p.is_none()) {
            return None;
        }
        let bytes: Vec<u8> = self.parts.drain(..).flatten().flatten().collect();
        self.done_id = Some(id);
        Content::from_parts(kind, bytes)
    }

    /// If the transfer in progress has gone quiet for `quiet`, the chunks to
    /// ask the sender for again. Gives up (and forgets) after a while.
    pub fn stalled(&mut self, quiet: Duration) -> Option<(u32, Vec<u16>)> {
        let last = self.last_rx?;
        if self.parts.is_empty() {
            return None;
        }
        if last.elapsed() > GIVE_UP {
            self.parts.clear();
            return None;
        }
        if last.elapsed() < quiet || self.last_nack.is_some_and(|t| t.elapsed() < quiet) {
            return None;
        }
        self.last_nack = Some(Instant::now());
        let missing: Vec<u16> = self
            .parts
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_none())
            .map(|(i, _)| i as u16)
            .take(NACK_MAX)
            .collect();
        Some((self.id, missing))
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

    /// True when the clipboard changed since the last call and should be
    /// sent; `send` false only records it (not connected), and the very first
    /// look is only recorded.
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

    fn deliver(c: &Content, skip: &[usize]) -> (Assembler, Option<Content>) {
        let mut a = Assembler::default();
        let mut out = None;
        for (i, e) in split(7, c).unwrap().iter().enumerate().rev() {
            if skip.contains(&i) {
                continue;
            }
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
        (a, out)
    }

    #[test]
    fn roundtrip_out_of_order() {
        let text = Content::Text("héllo wörld ".repeat(500));
        assert_eq!(deliver(&text, &[]).1.unwrap(), text);
        let mut png = PNG_MAGIC.to_vec();
        png.extend((0..50_000u32).map(|i| i as u8));
        let img = Content::Png(png);
        assert_eq!(deliver(&img, &[]).1.unwrap(), img);
        let files = Content::Files(vec![
            FileItem {
                name: "a.txt".into(),
                data: vec![1; 5000],
            },
            FileItem {
                name: "b.bin".into(),
                data: vec![],
            },
        ]);
        assert_eq!(deliver(&files, &[]).1.unwrap(), files);
    }

    #[test]
    fn lost_chunks_are_repaired() {
        let c = Content::Png([PNG_MAGIC, &[7u8; 9000]].concat());
        let (mut a, out) = deliver(&c, &[3, 9]);
        assert!(out.is_none());
        let (id, missing) = a.stalled(Duration::ZERO).unwrap();
        assert_eq!(missing, vec![3, 9]);
        let ob = Outbox::new(7, split(7, &c).unwrap());
        let mut done = None;
        for e in ob.resend(id, &missing) {
            if let Event::Clip {
                id,
                kind,
                idx,
                count,
                data,
            } = e
            {
                done = a.push(id, kind, idx, count, &data);
            }
        }
        assert_eq!(done.unwrap(), c);
        // late duplicates don't start a phantom transfer
        assert!(a.push(7, 1, 0, 10, b"x").is_none());
        assert!(a.stalled(Duration::ZERO).is_none());
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("C:\\x\\a?b.txt"), "ab.txt");
        assert_eq!(sanitize_name(" .. "), "file");
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
