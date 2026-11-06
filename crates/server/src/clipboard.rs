//! Clipboard sharing (text, images, files): watches the local clipboard,
//! sends changes to the Deck, and applies what it sends back.

use crate::net::Shared;
use glidekm_core::clip::{self, Content, Outbox, Watcher};
use glidekm_core::event::Event;
use glidekm_core::{debug, warn};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(windows)]
mod sys {
    use super::Content;
    use std::path::PathBuf;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::UI::Shell::DragQueryFileW;

    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;

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

    /// Paths of files copied in Explorer (clipboard must be open).
    unsafe fn dropped_files() -> Vec<PathBuf> {
        let h = GetClipboardData(CF_HDROP);
        if h.is_null() {
            return Vec::new();
        }
        let n = DragQueryFileW(h, u32::MAX, null_mut(), 0);
        let mut out = Vec::new();
        for i in 0..n {
            let len = DragQueryFileW(h, i, null_mut(), 0) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(h, i, buf.as_mut_ptr(), buf.len() as u32) as usize;
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
        }
        out
    }

    /// What is on the clipboard. Files are returned as paths; reading them
    /// happens after the clipboard is closed again.
    pub enum Read {
        Files(Vec<PathBuf>),
        Content(Content),
    }

    pub fn get() -> Option<Read> {
        unsafe {
            let files = IsClipboardFormatAvailable(CF_HDROP) != 0;
            let text = IsClipboardFormatAvailable(CF_UNICODETEXT) != 0;
            let png = IsClipboardFormatAvailable(cf_png()) != 0;
            let dib = IsClipboardFormatAvailable(CF_DIB) != 0;
            if !(files || text || png || dib) || !open() {
                return None;
            }
            let mut out = None;
            if files {
                let f = dropped_files();
                if !f.is_empty() {
                    out = Some(Read::Files(f));
                }
            }
            if out.is_none() && text {
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
                        out = Some(Read::Content(Content::Text(super::clip::normalize(&t))));
                    }
                }
            }
            if out.is_none() && png {
                out = bytes_of(cf_png()).map(|mut b| {
                    // some apps pad the allocation
                    if let Some(end) = b.windows(8).rposition(|w| w == b"IEND\xaeB`\x82") {
                        b.truncate(end + 8);
                    }
                    Read::Content(Content::Png(b))
                });
            }
            if out.is_none() && dib {
                out = bytes_of(CF_DIB)
                    .and_then(|b| crate::png::from_dib(&b))
                    .map(|p| Read::Content(Content::Png(p)));
            }
            CloseClipboard();
            out
        }
    }

    fn put(fmt: u32, data: &[u8]) -> bool {
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

    /// Decode a PNG with GDI+ into a packed DIB (alpha flattened onto white).
    fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
        use windows_sys::Win32::Graphics::Gdi::{GetObjectW, DIBSECTION};
        use windows_sys::Win32::Graphics::GdiPlus::{
            GdipCreateBitmapFromStream, GdipCreateHBITMAPFromBitmap, GdipDisposeImage,
            GdiplusShutdown, GdiplusStartup, GdiplusStartupInput,
        };
        use windows_sys::Win32::UI::Shell::SHCreateMemStream;
        unsafe {
            // Copies the bytes; released below.
            let stream = SHCreateMemStream(png.as_ptr(), png.len() as u32);
            if stream.is_null() {
                return None;
            }
            let input = GdiplusStartupInput {
                GdiplusVersion: 1,
                DebugEventCallback: 0,
                SuppressBackgroundThread: 0,
                SuppressExternalCodecs: 0,
            };
            let mut token = 0usize;
            let mut out = None;
            if GdiplusStartup(&mut token, &input, null_mut()) == 0 {
                let mut bmp = null_mut();
                if GdipCreateBitmapFromStream(stream, &mut bmp) == 0 && !bmp.is_null() {
                    let mut hbm: HBITMAP = null_mut();
                    if GdipCreateHBITMAPFromBitmap(bmp, &mut hbm, 0xFFFF_FFFF) == 0
                        && !hbm.is_null()
                    {
                        let mut ds: DIBSECTION = std::mem::zeroed();
                        let want = std::mem::size_of::<DIBSECTION>() as i32;
                        let got = GetObjectW(hbm, want, &mut ds as *mut _ as *mut _);
                        let bits = ds.dsBm.bmBits as *const u8;
                        let len = (ds.dsBm.bmWidthBytes * ds.dsBm.bmHeight) as usize;
                        if got == want && !bits.is_null() && len > 0 {
                            let mut bi = ds.dsBmih;
                            bi.biCompression = 0; // BI_RGB
                            bi.biSizeImage = len as u32;
                            let mut dib = Vec::with_capacity(40 + len);
                            dib.extend_from_slice(std::slice::from_raw_parts(
                                &bi as *const _ as *const u8,
                                std::mem::size_of_val(&bi),
                            ));
                            dib.extend_from_slice(std::slice::from_raw_parts(bits, len));
                            out = Some(dib);
                        }
                        DeleteObject(hbm);
                    }
                    GdipDisposeImage(bmp as _);
                }
                GdiplusShutdown(token);
            }
            let unk = stream as *mut *const windows_sys::core::IUnknown_Vtbl;
            ((**unk).Release)(stream as _);
            out
        }
    }

    /// Allocate a movable global block holding `data`.
    unsafe fn alloc(data: &[u8]) -> Option<HANDLE> {
        let mem: HANDLE = GlobalAlloc(GMEM_MOVEABLE, data.len());
        if mem.is_null() {
            return None;
        }
        let p = GlobalLock(mem) as *mut u8;
        if p.is_null() {
            GlobalFree(mem);
            return None;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        GlobalUnlock(mem);
        Some(mem)
    }

    /// An image goes on the clipboard twice: as the registered "PNG" format
    /// (lossless, with alpha) and as a DIB, which Windows turns into the
    /// other bitmap formats that most apps (and .NET) paste.
    fn put_png(png: &[u8]) -> bool {
        unsafe {
            let Some(pm) = alloc(png) else { return false };
            let dm = png_to_dib(png).and_then(|d| alloc(&d));
            if !open() {
                GlobalFree(pm);
                if let Some(d) = dm {
                    GlobalFree(d);
                }
                return false;
            }
            EmptyClipboard();
            let ok = !SetClipboardData(cf_png(), pm).is_null();
            if !ok {
                GlobalFree(pm);
            }
            if let Some(d) = dm {
                if SetClipboardData(CF_DIB, d).is_null() {
                    GlobalFree(d);
                }
            }
            CloseClipboard();
            ok
        }
    }

    /// Put text or an image on the clipboard.
    pub fn set(c: &Content) -> bool {
        match c {
            Content::Text(t) => put(
                CF_UNICODETEXT,
                &t.replace('\n', "\r\n")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(|u| u.to_le_bytes())
                    .collect::<Vec<u8>>(),
            ),
            Content::Png(b) => put_png(b),
            Content::Files(_) => false,
        }
    }

    /// Put files on the clipboard, as if copied in Explorer.
    pub fn set_files(paths: &[PathBuf]) -> bool {
        // DROPFILES header: offset 20, point 0/0, not non-client, wide chars.
        let mut d: Vec<u8> = Vec::new();
        d.extend_from_slice(&20u32.to_le_bytes());
        d.extend_from_slice(&[0u8; 12]);
        d.extend_from_slice(&1u32.to_le_bytes());
        for p in paths {
            for u in p.to_string_lossy().encode_utf16().chain(std::iter::once(0)) {
                d.extend_from_slice(&u.to_le_bytes());
            }
        }
        d.extend_from_slice(&[0, 0]);
        put(CF_HDROP, &d)
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Content;
    use std::path::PathBuf;
    #[allow(dead_code)]
    pub enum Read {
        Files(Vec<PathBuf>),
        Content(Content),
    }
    pub fn seq() -> u32 {
        0
    }
    pub fn get() -> Option<Read> {
        None
    }
    pub fn set(_: &Content) -> bool {
        false
    }
    pub fn set_files(_: &[PathBuf]) -> bool {
        false
    }
}

pub fn spawn(shared: Arc<Shared>, tx: Sender<Event>) {
    let _ = std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || run(shared, tx));
}

/// Where received files go (emptied before each new transfer).
fn inbox() -> PathBuf {
    std::env::temp_dir().join("glidekm-clip")
}

/// Put received content on the clipboard; returns a short description.
fn apply(c: &Content) -> Option<String> {
    match c {
        Content::Files(f) => {
            let dir = inbox();
            let _ = std::fs::remove_dir_all(&dir);
            let paths = match clip::save_files(&dir, f) {
                Ok(p) => p,
                Err(e) => {
                    warn!("clipboard: cannot save received files: {e}");
                    return None;
                }
            };
            sys::set_files(&paths).then(|| {
                if f.len() == 1 {
                    format!("File \"{}\" copied - paste it with Ctrl+V", f[0].name)
                } else {
                    format!("{} files copied - paste them with Ctrl+V", f.len())
                }
            })
        }
        Content::Png(_) => sys::set(c).then(|| "Image copied - paste it with Ctrl+V".into()),
        Content::Text(t) => {
            sys::set(c).then(|| format!("Text copied ({} characters)", t.chars().count()))
        }
    }
}

fn run(shared: Arc<Shared>, tx: Sender<Event>) {
    let mut w = Watcher::default();
    let mut id = (glidekm_core::session::now_millis() as u32) | 1;
    let mut last_seq = None;
    let mut settle: Option<(u32, bool)> = None;
    let mut settled_since = Instant::now();
    let mut last_poll = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let alive = shared.alive.load(Ordering::Relaxed);

        // Ask the Deck again for chunks of an incoming transfer that stalled.
        let stalled = shared
            .clip_asm
            .lock()
            .unwrap()
            .stalled(Duration::from_millis(300));
        if let Some((tid, missing)) = stalled {
            debug!("clipboard: asking for {} missing chunks", missing.len());
            let _ = tx.send(Event::ClipNack { id: tid, missing });
        }

        // From the Deck.
        let incoming = shared.clip_in.lock().unwrap().take();
        if let Some(c) = incoming {
            w.set_remote(&c);
            if let Some(msg) = apply(&c) {
                debug!("clipboard: received {} bytes", c.size());
                if !matches!(c, Content::Text(_)) || c.size() > 8000 {
                    shared.set_notice("Received from your Steam Deck", &msg);
                }
            }
            last_seq = Some(sys::seq());
            continue;
        }

        if last_poll.elapsed() < Duration::from_millis(400) {
            continue;
        }
        last_poll = Instant::now();

        // To the Deck (only look when something was copied).
        let seq = sys::seq();
        if last_seq == Some(seq) {
            continue;
        }
        // Let the copying app finish (e.g. the Snipping Tool sets several
        // formats and a notification); reading mid-write can disturb it.
        if settle != Some((seq, true)) {
            if settle.map(|s| s.0) != Some(seq) {
                settled_since = Instant::now();
            }
            settle = Some((seq, false));
            if settled_since.elapsed() < Duration::from_millis(600) {
                continue;
            }
            settle = Some((seq, true));
        }
        last_seq = Some(seq);
        let cur = match sys::get() {
            Some(sys::Read::Content(c)) => c,
            Some(sys::Read::Files(paths)) => match clip::load_files(&paths) {
                Some(f) => Content::Files(f),
                None => {
                    if alive {
                        shared.set_notice(
                            "Not sent to your Steam Deck",
                            "Only files (not folders) up to 32 MB in total can be shared.",
                        );
                    }
                    continue;
                }
            },
            None => continue,
        };
        if !w.changed(&cur, alive) {
            continue;
        }
        let Some(chunks) = clip::split(id, &cur) else {
            continue;
        };
        debug!("clipboard: sending {} bytes", cur.size());
        *shared.clip_out.lock().unwrap() = Some(Outbox::new(id, chunks.clone()));
        id = id.wrapping_add(1);
        let total = chunks.len();
        let kind = match cur {
            Content::Text(_) => 0,
            Content::Png(_) => 1,
            Content::Files(_) => 2,
        };
        for (i, c) in chunks.into_iter().enumerate() {
            if tx.send(c).is_err() {
                return;
            }
            shared.set_xfer(true, kind, i + 1, total);
            if i % 4 == 3 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // Same rule as for received content: images and files always get a
        // notification (even a tiny file), text only when it is large.
        if !matches!(cur, Content::Text(_)) || cur.size() > 8000 {
            shared.set_notice("Sent to your Steam Deck", &sent_text(&cur));
        }
    }
}

fn sent_text(c: &Content) -> String {
    match c {
        Content::Files(f) if f.len() == 1 => format!("File \"{}\" sent", f[0].name),
        Content::Files(f) => format!("{} files sent", f.len()),
        Content::Png(_) => "Image sent".into(),
        Content::Text(_) => "Text sent".into(),
    }
}
