//! Windows notification-area icon, its menu, opening the settings page, and
//! the per-user "start with Windows" registry entry.

use crate::control::Status;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_TRAY: u32 = WM_APP + 5;
pub const ID_OPEN: usize = 1;
pub const ID_SWITCH: usize = 2;
pub const ID_QUIT: usize = 3;
const TRAY_ID: u32 = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn status_color(s: Status) -> u32 {
    match s {
        Status::Unconfigured => 0x8A919C,
        Status::Connecting => 0xD97706,
        Status::Online => 0x16A34A,
        Status::Active => 0x2563EB,
    }
}

/// Draw a 32x32 "monitor" glyph in the status color.
fn make_icon(status: Status) -> HICON {
    const N: usize = 32;
    let rgb = status_color(status);
    let solid = 0xFF00_0000 | rgb;
    let soft = 0x5500_0000 | rgb;
    let mut px = vec![0u32; N * N];
    let mut put = |x: usize, y: usize, c: u32| px[y * N + x] = c;
    // Screen: x 1..31, y 3..23, 3 px frame, rounded corners.
    for y in 3..23 {
        for x in 1..31 {
            let corner = (x == 1 || x == 30) && (y == 3 || y == 22);
            if corner {
                continue;
            }
            let frame = !(4..=27).contains(&x) || !(6..=19).contains(&y);
            put(x, y, if frame { solid } else { soft });
        }
    }
    // Stand and base.
    for y in 23..27 {
        for x in 13..19 {
            put(x, y, solid);
        }
    }
    for y in 27..30 {
        for x in 8..24 {
            put(x, y, solid);
        }
    }
    unsafe {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = N as i32;
        bi.bmiHeader.biHeight = -(N as i32); // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(
            std::ptr::null_mut(),
            &bi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if color.is_null() || bits.is_null() {
            return LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, N * N);
        let mask_bits = [0u8; N * N / 8];
        let mask = CreateBitmap(N as i32, N as i32, 1, 1, mask_bits.as_ptr() as _);
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(color as _);
        DeleteObject(mask as _);
        if icon.is_null() {
            LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
        } else {
            icon
        }
    }
}

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = TRAY_ID;
    n
}

fn set_tip(n: &mut NOTIFYICONDATAW, tip: &str) {
    let w: Vec<u16> = tip.encode_utf16().take(n.szTip.len() - 1).collect();
    n.szTip[..w.len()].copy_from_slice(&w);
    n.szTip[w.len()] = 0;
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    last: Option<(Status, String)>,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Tray {
        Tray {
            hwnd,
            icon: std::ptr::null_mut(),
            last: None,
        }
    }

    /// Add (or re-add after Explorer restarts) the icon.
    pub fn add(&mut self, status: Status, tip: &str) {
        self.last = None;
        unsafe {
            let mut n = nid(self.hwnd);
            Shell_NotifyIconW(NIM_DELETE, &n);
            n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            n.uCallbackMessage = WM_APP_TRAY;
            self.replace_icon(status);
            n.hIcon = self.icon;
            set_tip(&mut n, tip);
            Shell_NotifyIconW(NIM_ADD, &n);
        }
        self.last = Some((status, tip.to_string()));
    }

    fn replace_icon(&mut self, status: Status) {
        let old = self.icon;
        self.icon = make_icon(status);
        if !old.is_null() {
            unsafe { DestroyIcon(old) };
        }
    }

    /// Update icon/tooltip if anything changed.
    pub fn update(&mut self, status: Status, tip: &str) {
        if self
            .last
            .as_ref()
            .is_some_and(|(s, t)| *s == status && t == tip)
        {
            return;
        }
        let icon_changed = self.last.as_ref().is_none_or(|(s, _)| *s != status);
        unsafe {
            let mut n = nid(self.hwnd);
            n.uFlags = NIF_TIP;
            if icon_changed {
                self.replace_icon(status);
                n.uFlags |= NIF_ICON;
                n.hIcon = self.icon;
            }
            set_tip(&mut n, tip);
            Shell_NotifyIconW(NIM_MODIFY, &n);
        }
        self.last = Some((status, tip.to_string()));
    }

    pub fn remove(&mut self) {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &nid(self.hwnd));
            if !self.icon.is_null() {
                DestroyIcon(self.icon);
                self.icon = std::ptr::null_mut();
            }
        }
    }

    /// Show the context menu at the cursor; returns the chosen command id.
    pub fn menu(&self, can_switch: bool, in_deck: bool) -> usize {
        unsafe {
            let m = CreatePopupMenu();
            let open = wide("Open settings");
            let switch = wide(if in_deck {
                "Back to PC"
            } else {
                "Switch to Deck"
            });
            let quit = wide("Quit glidekm");
            AppendMenuW(m, MF_STRING, ID_OPEN, open.as_ptr());
            AppendMenuW(
                m,
                MF_STRING | if can_switch { 0 } else { MF_GRAYED },
                ID_SWITCH,
                switch.as_ptr(),
            );
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(m, MF_STRING, ID_QUIT, quit.as_ptr());
            SetMenuDefaultItem(m, ID_OPEN as u32, 0);
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // Required so the menu closes when clicking elsewhere.
            SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                m,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                std::ptr::null(),
            );
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(m);
            cmd as usize
        }
    }
}

pub fn open_url(url: &str) {
    if url.is_empty() {
        return;
    }
    let verb = wide("open");
    let u = wide(url);
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            u.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

// ---------------------------------------------------------------- autostart

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "glidekm";

pub fn autostart_enabled() -> bool {
    let key = wide(RUN_KEY);
    let val = wide(RUN_VALUE);
    let mut size = 0u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        ) == ERROR_SUCCESS
    }
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    let key = wide(RUN_KEY);
    let val = wide(RUN_VALUE);
    let r = unsafe {
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let data = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                val.as_ptr(),
                REG_SZ,
                data.as_ptr() as _,
                (data.len() * 2) as u32,
            )
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr());
            if r == ERROR_FILE_NOT_FOUND {
                ERROR_SUCCESS
            } else {
                r
            }
        }
    };
    if r == ERROR_SUCCESS {
        glidekm_core::info!("start with Windows: {}", if on { "on" } else { "off" });
        Ok(())
    } else {
        Err(format!("registry error {r}"))
    }
}
