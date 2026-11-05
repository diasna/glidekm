//! Text clipboard sharing: polls the local clipboard, sends changes to the
//! Deck, and applies text received from it.

use crate::net::Shared;
use glidekm_core::clip::{self, Watcher};
use glidekm_core::debug;
use glidekm_core::event::Event;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
mod sys {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    const CF_UNICODETEXT: u32 = 13;

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

    pub fn get() -> Option<String> {
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 || !open() {
                return None;
            }
            let mut out = None;
            let h = GetClipboardData(CF_UNICODETEXT);
            if !h.is_null() {
                let p = GlobalLock(h) as *const u16;
                if !p.is_null() {
                    let mut n = 0;
                    while *p.add(n) != 0 {
                        n += 1;
                    }
                    out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                    GlobalUnlock(h);
                }
            }
            CloseClipboard();
            out
        }
    }

    pub fn set(text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let mem: HANDLE = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if mem.is_null() {
                return false;
            }
            let p = GlobalLock(mem) as *mut u16;
            if p.is_null() {
                GlobalFree(mem);
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            GlobalUnlock(mem);
            if !open() {
                GlobalFree(mem);
                return false;
            }
            EmptyClipboard();
            let ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
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
    pub fn get() -> Option<String> {
        None
    }
    pub fn set(_: &str) -> bool {
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
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let alive = shared.alive.load(Ordering::Relaxed);

        // From the Deck.
        let incoming = shared.clip_text.lock().unwrap().take();
        if let Some(t) = incoming {
            let t = clip::normalize(&t);
            w.set_remote(&t);
            if sys::set(&t.replace('\n', "\r\n")) {
                debug!("clipboard: received {} bytes", t.len());
            }
            continue;
        }

        // To the Deck.
        if let Some(cur) = sys::get() {
            let cur = clip::normalize(&cur);
            if w.changed(&cur, alive) {
                if let Some(chunks) = clip::split(id, &cur) {
                    id = id.wrapping_add(1);
                    debug!("clipboard: sending {} bytes", cur.len());
                    for c in chunks {
                        if tx.send(c).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }
    }
}
