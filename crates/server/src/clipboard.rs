//! Clipboard sharing (text and images): watches the local clipboard, sends
//! changes to the Deck, and applies what it sends back.

use crate::net::Shared;
use glidekm_core::clip::{self, Content, Watcher};
use glidekm_core::debug;
use glidekm_core::event::Event;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
mod sys {
    use super::Content;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };

    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;

    /// The registered "PNG" clipboard format (browsers, Paint, Office).
    fn cf_png() -> u32 {
        static F: OnceLock<u32> = OnceLock::new();
        *F.get_or_init(|| {
            let name: Vec<u16> = "PNG\0".encode_utf16().collect();
            unsafe { RegisterClipboardFormatW(name.as_ptr()) }
        })
    }

    /// Changes whenever anything puts something on the clipboard.
    pub fn seq() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    /// Open the clipboard, retrying briefly (other apps hold it while copying).
    fn open() -> bool {
        for _ in 0..10 {
            if unsafe { OpenClipboard(null_mut()) } != 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// Bytes of a clipboard format (clipboard must be open).
    unsafe fn bytes_of(fmt: u32) -> Option<Vec<u8>> {
        let h = GetClipboardData(fmt);
        if h.is_null() {
            return None;
        }
        let n = GlobalSize(h);
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, n).to_vec();
        GlobalUnlock(h);
        Some(v)
    }

    pub fn get() -> Option<Content> {
        unsafe {
            let text = IsClipboardFormatAvailable(CF_UNICODETEXT) != 0;
            let png = IsClipboardFormatAvailable(cf_png()) != 0;
            let dib = IsClipboardFormatAvailable(CF_DIB) != 0;
            if !(text || png || dib) || !open() {
                return None;
            }
            let mut out = None;
            if text {
                if let Some(b) = bytes_of(CF_UNICODETEXT) {
                    let w: Vec<u16> = b
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .take_while(|&c| c != 0)
                        .collect();
                    let t = String::from_utf16_lossy(&w);
                    if !t.is_empty() {
                        out = Some(Content::Text(super::clip::normalize(&t)));
                    }
                }
            }
            if out.is_none() && png {
                out = bytes_of(cf_png()).map(|mut b| {
                    // some apps pad the allocation
                    if let Some(end) = b.windows(8).rposition(|w| w == b"IEND\xaeB`\x82") {
                        b.truncate(end + 8);
                    }
                    Content::Png(b)
                });
            }
            if out.is_none() && dib {
                out = bytes_of(CF_DIB)
                    .and_then(|b| crate::png::from_dib(&b))
                    .map(Content::Png);
            }
            CloseClipboard();
            out
        }
    }

    pub fn set(c: &Content) -> bool {
        let (fmt, data): (u32, Vec<u8>) = match c {
            Content::Text(t) => (
                CF_UNICODETEXT,
                t.replace('\n', "\r\n")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(|u| u.to_le_bytes())
                    .collect(),
            ),
            Content::Png(b) => (cf_png(), b.clone()),
        };
        unsafe {
            let mem: HANDLE = GlobalAlloc(GMEM_MOVEABLE, data.len());
            if mem.is_null() {
                return false;
            }
            let p = GlobalLock(mem) as *mut u8;
            if p.is_null() {
                GlobalFree(mem);
                return false;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            GlobalUnlock(mem);
            if !open() {
                GlobalFree(mem);
                return false;
            }
            EmptyClipboard();
            let ok = !SetClipboardData(fmt, mem).is_null();
            if !ok {
                GlobalFree(mem);
            }
            CloseClipboard();
            ok
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Content;
    pub fn seq() -> u32 {
        0
    }
    pub fn get() -> Option<Content> {
        None
    }
    pub fn set(_: &Content) -> bool {
        false
    }
}

pub fn spawn(shared: Arc<Shared>, tx: Sender<Event>) {
    let _ = std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || run(shared, tx));
}

fn run(shared: Arc<Shared>, tx: Sender<Event>) {
    let mut w = Watcher::default();
    let mut id = (glidekm_core::session::now_millis() as u32) | 1;
    let mut last_seq = None;
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let alive = shared.alive.load(Ordering::Relaxed);

        // From the Deck.
        let incoming = shared.clip_in.lock().unwrap().take();
        if let Some(c) = incoming {
            w.set_remote(&c);
            if sys::set(&c) {
                debug!("clipboard: received {} bytes", c.bytes().len());
            }
            last_seq = Some(sys::seq());
            continue;
        }

        // To the Deck (only look when something was copied).
        let seq = sys::seq();
        if last_seq == Some(seq) {
            continue;
        }
        last_seq = Some(seq);
        if let Some(cur) = sys::get() {
            if w.changed(&cur, alive) {
                if let Some(chunks) = clip::split(id, &cur) {
                    id = id.wrapping_add(1);
                    debug!("clipboard: sending {} bytes", cur.bytes().len());
                    for c in chunks {
                        if tx.send(c).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
    }
}
