//! Native Win32 settings window.
//!
//! Runs on its own UI thread so the low-level input hooks (on the main
//! thread) are never delayed by UI work. Minimizing or closing only hides
//! the window; the server keeps running in the tray.

use crate::control::{Control, ServerConfig, Status};
use crate::hotkey::{Hotkey, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use crate::layout::{Layout, Side};
use crate::tray;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, ReleaseCapture, SetCapture, SetFocus,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// While the settings window is focused, glidekm's own hotkeys are paused so
/// new combinations can be recorded.
pub static PAUSE_HOTKEYS: AtomicBool = AtomicBool::new(false);
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static SCAN_RESULT: Mutex<Option<Vec<(String, String)>>> = Mutex::new(None);

const WM_APP_SHOW: u32 = WM_APP + 20;
const WM_APP_SCAN_DONE: u32 = WM_APP + 21;
const WM_APP_SET_ADDR: u32 = WM_APP + 22;
const TIMER_REFRESH: usize = 1;
const CLASS: &str = "glidekm-settings";
const PANEL_CLASS: &str = "glidekm-arrange";

// Control ids.
const ID_SWITCH: u16 = 100;
const ID_PANEL: u16 = 101;
const ID_SIDE: u16 = 102;
const ID_OFFSET: u16 = 103;
const ID_EDGE: u16 = 104;
const ID_ADDR: u16 = 110;
const ID_SCAN: u16 = 111;
const ID_PASS: u16 = 112;
const ID_SHOWPASS: u16 = 113;
const ID_GENPASS: u16 = 114;
const ID_SPEED: u16 = 120;
const ID_SPEEDVAL: u16 = 121;
const ID_HK_SWITCH: u16 = 122;
const ID_HK_CAPTURE: u16 = 123;
const ID_IGNINJ: u16 = 124;
const ID_AUTOSTART: u16 = 125;
const ID_VERSION: u16 = 130;
const ID_OPEN_CFG: u16 = 131;
const ID_OPEN_LOG: u16 = 132;
const ID_MSG: u16 = 140;
const ID_REVERT: u16 = 141;
const ID_SAVE: u16 = 142;

// Static control styles and trackbar messages (not all exported by windows-sys).
const SS_LEFT: u32 = 0x0;
const SS_RIGHT: u32 = 0x2;
const SS_ENDELLIPSIS: u32 = 0x4000;
const TBM_GETPOS: u32 = WM_USER;

fn mul_div(v: i32, num: i32, den: i32) -> i32 {
    ((v as i64 * num as i64 + den as i64 / 2) / den as i64) as i32
}

const CLIENT_W: i32 = 880;
const CLIENT_H: i32 = 582;
const HEADER_H: i32 = 44;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Open (or bring back) the settings window.
pub fn show(ctl: Arc<Control>) {
    let h = WINDOW.load(Ordering::SeqCst);
    if h != 0 {
        unsafe { PostMessageW(h as HWND, WM_APP_SHOW, 0, 0) };
        return;
    }
    let _ = std::thread::Builder::new()
        .name("settings-ui".into())
        .spawn(move || unsafe { run(ctl) });
}

// ---------------------------------------------------------------- state

#[derive(Clone, PartialEq)]
struct Draft {
    client: String,
    pass: String,
    side: Side,
    offset: i32,
    scale_pct: u32,
    hk_switch: String,
    hk_capture: String,
    ignore_injected: bool,
}

impl Draft {
    fn from(c: &ServerConfig) -> Draft {
        Draft {
            client: c.client.clone(),
            pass: String::new(),
            side: c.side,
            offset: c.offset,
            scale_pct: (c.scale * 100.0).round() as u32,
            hk_switch: c.hotkey_switch.clone(),
            hk_capture: c.hotkey_capture.clone(),
            ignore_injected: c.ignore_injected,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MsgKind {
    Info,
    Ok,
    Err,
}

struct View {
    k: f64,
    ox: f64,
    oy: f64,
}

struct Ui {
    ctl: Arc<Control>,
    hwnd: HWND,
    panel: HWND,
    dpi: u32,
    font: HFONT,
    head_font: HFONT,
    ctrls: Vec<(HWND, [i32; 4])>,
    saved: Draft,
    draft: Draft,
    /// Suppresses change notifications while we set control values.
    updating: bool,
    drag: Option<(f64, f64)>,
    view: View,
    status: (Status, String, bool),
    msg: (String, MsgKind),
    show_pass: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

impl Ui {
    fn s(&self, v: i32) -> i32 {
        mul_div(v, self.dpi as i32, 96)
    }

    fn item(&self, id: u16) -> HWND {
        unsafe { GetDlgItem(self.hwnd, id as i32) }
    }

    fn layout_for(&self) -> Layout {
        let mons = self.ctl.monitors.lock().unwrap().clone();
        let (w, h) = self.ctl.shared.size().unwrap_or((1280, 800));
        Layout {
            monitors: if mons.is_empty() {
                vec![crate::layout::Rect {
                    l: 0,
                    t: 0,
                    r: 1920,
                    b: 1080,
                }]
            } else {
                mons
            },
            side: self.draft.side,
            offset: self.draft.offset,
            scale: self.draft.scale_pct as f64 / 100.0,
            client_w: w,
            client_h: h,
        }
    }

    fn dirty(&self) -> bool {
        self.draft != self.saved
    }

    fn set_text(&self, id: u16, t: &str) {
        let w = wide(t);
        unsafe { SetWindowTextW(self.item(id), w.as_ptr()) };
    }

    fn text(&self, id: u16) -> String {
        unsafe {
            let h = self.item(id);
            let n = GetWindowTextLengthW(h);
            let mut buf = vec![0u16; n as usize + 1];
            let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
            String::from_utf16_lossy(&buf[..got.max(0) as usize])
        }
    }

    fn set_check(&self, id: u16, on: bool) {
        unsafe { SendMessageW(self.item(id), BM_SETCHECK, on as usize, 0) };
    }

    fn checked(&self, id: u16) -> bool {
        unsafe { SendMessageW(self.item(id), BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
    }

    fn enable(&self, id: u16, on: bool) {
        unsafe { EnableWindow(self.item(id), on as i32) };
    }

    fn set_msg(&mut self, t: &str, k: MsgKind) {
        self.msg = (t.to_string(), k);
        self.set_text(ID_MSG, t);
        unsafe { InvalidateRect(self.item(ID_MSG), std::ptr::null(), 1) };
    }

    /// Push the draft into the controls.
    fn fill(&mut self) {
        self.updating = true;
        let d = self.draft.clone();
        self.set_text(ID_ADDR, &d.client);
        self.set_text(ID_PASS, &d.pass);
        let cue = wide(if self.ctl.cfg.lock().unwrap().passphrase.is_empty() {
            "required - same as on the Deck"
        } else {
            "•••••••• (unchanged)"
        });
        unsafe {
            SendMessageW(
                self.item(ID_PASS),
                EM_SETCUEBANNER,
                1,
                cue.as_ptr() as isize,
            );
            let side_idx = match d.side {
                Side::Right => 0,
                Side::Left => 1,
                Side::Top => 2,
                Side::Bottom => 3,
            };
            SendMessageW(self.item(ID_SIDE), CB_SETCURSEL, side_idx, 0);
            SendMessageW(self.item(ID_SPEED), TBM_SETPOS, 1, d.scale_pct as isize);
            set_hotkey(self.item(ID_HK_SWITCH), &d.hk_switch);
            set_hotkey(self.item(ID_HK_CAPTURE), &d.hk_capture);
        }
        self.set_text(ID_OFFSET, &d.offset.to_string());
        self.set_text(ID_SPEEDVAL, &format!("{:.2}", d.scale_pct as f64 / 100.0));
        self.set_check(ID_IGNINJ, d.ignore_injected);
        let auto = self.ctl.platform.lock().unwrap().autostart();
        self.set_check(ID_AUTOSTART, auto.unwrap_or(false));
        self.enable(ID_AUTOSTART, auto.is_some());
        self.updating = false;
        self.after_change();
    }

    fn after_change(&mut self) {
        let d = self.dirty();
        self.enable(ID_SAVE, d);
        self.enable(ID_REVERT, d);
        if d && self.msg.1 != MsgKind::Err {
            self.set_msg("Unsaved changes", MsgKind::Info);
        } else if !d && self.msg.0 == "Unsaved changes" {
            self.set_msg("", MsgKind::Info);
        }
        self.update_edge_text();
        unsafe { InvalidateRect(self.panel, std::ptr::null(), 0) };
    }

    fn update_edge_text(&self) {
        let l = self.layout_for();
        let total: f64 = l.shared_segments().iter().map(|(a, b)| b - a).sum();
        let known = self.ctl.shared.size().is_some();
        let t = if total <= 0.0 {
            "The Deck doesn't touch any monitor here - the cursor can't reach it.".to_string()
        } else {
            format!(
                "Shared edge: {} px{}",
                total.round(),
                if known {
                    ""
                } else {
                    " (Deck size assumed until it connects)"
                }
            )
        };
        self.set_text(ID_EDGE, &t);
    }

    /// Read a changed control back into the draft.
    fn on_command(&mut self, id: u16, code: u32) {
        if self.updating {
            return;
        }
        match (id, code) {
            (ID_ADDR, CBN_EDITCHANGE) => self.draft.client = self.text(ID_ADDR).trim().to_string(),
            (ID_ADDR, CBN_SELCHANGE) => unsafe {
                let i = SendMessageW(self.item(ID_ADDR), CB_GETCURSEL, 0, 0);
                if i >= 0 {
                    let mut buf = [0u16; 256];
                    SendMessageW(
                        self.item(ID_ADDR),
                        CB_GETLBTEXT,
                        i as usize,
                        buf.as_mut_ptr() as isize,
                    );
                    let s = String::from_utf16_lossy(
                        &buf[..buf.iter().position(|c| *c == 0).unwrap_or(0)],
                    );
                    // List entries look like "steamdeck - 192.168.1.50"; keep the address.
                    self.draft.client = s.rsplit(" - ").next().unwrap_or(&s).trim().to_string();
                    // The combo writes the full list text into its edit box
                    // after this notification; replace it with the address.
                    PostMessageW(self.hwnd, WM_APP_SET_ADDR, 0, 0);
                }
            },
            (ID_PASS, EN_CHANGE) => self.draft.pass = self.text(ID_PASS),
            (ID_OFFSET, EN_CHANGE) => {
                if let Ok(v) = self.text(ID_OFFSET).trim().parse::<i32>() {
                    self.draft.offset = v.clamp(-100_000, 100_000);
                }
            }
            (ID_SIDE, CBN_SELCHANGE) => {
                let i = unsafe { SendMessageW(self.item(ID_SIDE), CB_GETCURSEL, 0, 0) };
                self.draft.side =
                    [Side::Right, Side::Left, Side::Top, Side::Bottom][i.clamp(0, 3) as usize];
            }
            (ID_IGNINJ, BN_CLICKED) => self.draft.ignore_injected = self.checked(ID_IGNINJ),
            (ID_AUTOSTART, BN_CLICKED) => {
                let on = self.checked(ID_AUTOSTART);
                let r = self.ctl.platform.lock().unwrap().set_autostart(on);
                match r {
                    Ok(()) => self.set_msg(
                        if on {
                            "glidekm will start with Windows."
                        } else {
                            "Removed from startup."
                        },
                        MsgKind::Ok,
                    ),
                    Err(e) => {
                        self.set_check(ID_AUTOSTART, !on);
                        self.set_msg(&e, MsgKind::Err);
                    }
                }
                return;
            }
            (ID_SHOWPASS, BN_CLICKED) => {
                self.show_pass = !self.show_pass;
                unsafe {
                    SendMessageW(
                        self.item(ID_PASS),
                        EM_SETPASSWORDCHAR,
                        if self.show_pass { 0 } else { 0x25CF },
                        0,
                    );
                    InvalidateRect(self.item(ID_PASS), std::ptr::null(), 1);
                }
                self.set_text(ID_SHOWPASS, if self.show_pass { "Hide" } else { "Show" });
                return;
            }
            (ID_GENPASS, BN_CLICKED) => {
                let p = generate_passphrase();
                self.draft.pass = p.clone();
                self.updating = true;
                self.set_text(ID_PASS, &p);
                self.updating = false;
                if !self.show_pass {
                    self.on_command(ID_SHOWPASS, BN_CLICKED);
                }
            }
            (ID_SCAN, BN_CLICKED) => {
                self.enable(ID_SCAN, false);
                self.set_text(ID_SCAN, "Scanning…");
                let ctl = self.ctl.clone();
                let hwnd = self.hwnd as isize;
                std::thread::spawn(move || {
                    let found = crate::scan::scan(&ctl);
                    *SCAN_RESULT.lock().unwrap() = Some(found);
                    unsafe { PostMessageW(hwnd as HWND, WM_APP_SCAN_DONE, 0, 0) };
                });
                return;
            }
            (ID_SWITCH, BN_CLICKED) => {
                self.ctl.platform.lock().unwrap().toggle_switch();
                return;
            }
            (ID_OPEN_CFG, BN_CLICKED) => {
                if let Some(dir) = self.ctl.path.parent() {
                    tray::open_url(&dir.display().to_string());
                }
                return;
            }
            (ID_OPEN_LOG, BN_CLICKED) => {
                let p = crate::exe_dir().join("glidekm-server.log");
                tray::open_url(&p.display().to_string());
                return;
            }
            (ID_REVERT, BN_CLICKED) => {
                self.draft = self.saved.clone();
                self.set_msg("", MsgKind::Info);
                self.fill();
                return;
            }
            (ID_SAVE, BN_CLICKED) => {
                self.save();
                return;
            }
            _ => return,
        }
        self.after_change();
    }

    fn on_scan_done(&mut self) {
        self.enable(ID_SCAN, true);
        self.set_text(ID_SCAN, "Scan");
        let found = SCAN_RESULT.lock().unwrap().take().unwrap_or_default();
        let combo = self.item(ID_ADDR);
        unsafe {
            SendMessageW(combo, CB_RESETCONTENT, 0, 0);
            for (addr, name) in &found {
                let w = wide(&format!(
                    "{} - {addr}",
                    if name.is_empty() { "Deck" } else { name }
                ));
                SendMessageW(combo, CB_ADDSTRING, 0, w.as_ptr() as isize);
            }
        }
        // CB_RESETCONTENT clears the edit text too: restore it.
        self.updating = true;
        self.set_text(ID_ADDR, &self.draft.client.clone());
        self.updating = false;
        if found.is_empty() {
            self.set_msg(
                "No Deck answered. Is glidekm running on it and on the same network? You can type its IP.",
                MsgKind::Err,
            );
        } else {
            self.set_msg(
                &format!("Found {} - pick one from the list.", found.len()),
                MsgKind::Ok,
            );
            unsafe {
                SetFocus(combo);
                SendMessageW(combo, CB_SHOWDROPDOWN, 1, 0);
            }
        }
    }

    fn save(&mut self) {
        // Hotkey controls don't notify; read them now.
        self.read_hotkeys();
        let cur = self.ctl.cfg.lock().unwrap().clone();
        let d = self.draft.clone();
        let new = ServerConfig {
            client: d.client.trim().to_string(),
            passphrase: if d.pass.is_empty() {
                cur.passphrase.clone()
            } else {
                d.pass.clone()
            },
            side: d.side,
            offset: d.offset,
            scale: d.scale_pct as f64 / 100.0,
            hotkey_switch: d.hk_switch.clone(),
            hotkey_capture: d.hk_capture.clone(),
            ignore_injected: d.ignore_injected,
        };
        match self.ctl.apply(new) {
            Ok(()) => {
                self.draft.pass.clear();
                self.saved = self.draft.clone();
                if self.show_pass {
                    self.on_command(ID_SHOWPASS, BN_CLICKED);
                }
                self.fill();
                self.set_msg("Saved - applied immediately.", MsgKind::Ok);
            }
            Err(e) => self.set_msg(&e, MsgKind::Err),
        }
    }

    fn read_hotkeys(&mut self) {
        for (id, field) in [(ID_HK_SWITCH, 0), (ID_HK_CAPTURE, 1)] {
            if let Some(s) = get_hotkey(self.item(id)) {
                let slot = if field == 0 {
                    &mut self.draft.hk_switch
                } else {
                    &mut self.draft.hk_capture
                };
                if *slot != s {
                    *slot = s;
                }
            }
        }
    }

    /// Periodic refresh: status header, switch button, hotkey polling.
    fn tick(&mut self) {
        let st = self.ctl.status();
        let text = self.ctl.status_text();
        let capture = self.ctl.shared.capture.load(Ordering::Relaxed);
        if (st, text.clone(), capture) != self.status {
            let size_changed = self.status.0 != st;
            self.status = (st, text, capture);
            unsafe {
                let r = RECT {
                    left: 0,
                    top: 0,
                    right: self.s(CLIENT_W),
                    bottom: self.s(HEADER_H),
                };
                InvalidateRect(self.hwnd, &r, 1);
            }
            self.enable(ID_SWITCH, matches!(st, Status::Online | Status::Active));
            self.set_text(
                ID_SWITCH,
                if st == Status::Active {
                    "Back to PC"
                } else {
                    "Switch to Deck"
                },
            );
            if size_changed {
                self.update_edge_text();
                unsafe { InvalidateRect(self.panel, std::ptr::null(), 0) };
            }
        }
        let before = (self.draft.hk_switch.clone(), self.draft.hk_capture.clone());
        self.read_hotkeys();
        if before != (self.draft.hk_switch.clone(), self.draft.hk_capture.clone()) {
            self.after_change();
        }
    }

    // ------------------------------------------------------------ layout

    fn add(&mut self, class: &str, text: &str, style: u32, ex: u32, id: u16, r: [i32; 4]) -> HWND {
        let c = wide(class);
        let t = wide(text);
        let h = unsafe {
            CreateWindowExW(
                ex,
                c.as_ptr(),
                t.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                0,
                0,
                self.hwnd,
                id as usize as HMENU,
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        self.ctrls.push((h, r));
        h
    }

    fn build(&mut self) {
        let lbl = |s: &mut Ui, t: &str, r: [i32; 4]| {
            s.add("STATIC", t, SS_LEFT, 0, 0xFFFF, r);
        };
        let group = |s: &mut Ui, t: &str, r: [i32; 4]| {
            s.add("BUTTON", t, BS_GROUPBOX as u32, 0, 0xFFFF, r);
        };
        let btn = WS_TABSTOP | BS_PUSHBUTTON as u32;
        let chk = WS_TABSTOP | BS_AUTOCHECKBOX as u32;

        self.add(
            "BUTTON",
            "Switch to Deck",
            btn,
            0,
            ID_SWITCH,
            [744, 8, 124, 28],
        );

        group(self, "Arrangement", [12, 52, 440, 300]);
        let p = self.add(PANEL_CLASS, "", 0, 0, ID_PANEL, [24, 76, 416, 212]);
        self.panel = p;
        lbl(self, "Side", [24, 300, 36, 22]);
        let side = self.add(
            "COMBOBOX",
            "",
            WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
            0,
            ID_SIDE,
            [62, 296, 104, 160],
        );
        for s in ["Right", "Left", "Top", "Bottom"] {
            let w = wide(s);
            unsafe { SendMessageW(side, CB_ADDSTRING, 0, w.as_ptr() as isize) };
        }
        lbl(self, "Offset", [180, 300, 44, 22]);
        self.add(
            "EDIT",
            "0",
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
            ID_OFFSET,
            [226, 296, 76, 24],
        );
        lbl(self, "px  (drag the Deck above)", [308, 300, 140, 22]);
        self.add("STATIC", "", SS_LEFT, 0, ID_EDGE, [24, 326, 420, 20]);

        group(self, "Connection", [12, 360, 440, 170]);
        lbl(self, "Steam Deck address (IP or name)", [24, 384, 410, 18]);
        self.add(
            "COMBOBOX",
            "",
            WS_TABSTOP | CBS_DROPDOWN as u32 | CBS_AUTOHSCROLL as u32 | WS_VSCROLL,
            0,
            ID_ADDR,
            [24, 404, 330, 200],
        );
        self.add("BUTTON", "Scan", btn, 0, ID_SCAN, [360, 403, 80, 26]);
        lbl(self, "Passphrase", [24, 438, 410, 18]);
        self.add(
            "EDIT",
            "",
            WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_PASSWORD as u32,
            WS_EX_CLIENTEDGE,
            ID_PASS,
            [24, 458, 222, 24],
        );
        self.add("BUTTON", "Show", btn, 0, ID_SHOWPASS, [252, 457, 64, 26]);
        self.add(
            "BUTTON",
            "Generate",
            btn,
            0,
            ID_GENPASS,
            [322, 457, 118, 26],
        );
        lbl(
            self,
            "Use the same passphrase on the Deck. It encrypts everything you type.",
            [24, 490, 416, 34],
        );

        group(self, "Behavior", [464, 52, 404, 300]);
        lbl(self, "Pointer speed on the Deck", [476, 76, 380, 18]);
        lbl(self, "faster", [476, 101, 44, 20]);
        let tb = self.add(
            "msctls_trackbar32",
            "",
            WS_TABSTOP | TBS_HORZ | TBS_NOTICKS,
            0,
            ID_SPEED,
            [520, 96, 258, 28],
        );
        unsafe {
            SendMessageW(tb, TBM_SETRANGEMIN, 0, 50);
            SendMessageW(tb, TBM_SETRANGEMAX, 0, 300);
            SendMessageW(tb, TBM_SETPAGESIZE, 0, 10);
        }
        lbl(self, "slower", [780, 101, 44, 20]);
        self.add(
            "STATIC",
            "1.00",
            SS_RIGHT,
            0,
            ID_SPEEDVAL,
            [824, 101, 36, 20],
        );
        lbl(
            self,
            "Switch between PC and Deck (hotkey)",
            [476, 134, 380, 18],
        );
        self.add(
            "msctls_hotkey32",
            "",
            WS_TABSTOP,
            WS_EX_CLIENTEDGE,
            ID_HK_SWITCH,
            [476, 154, 220, 24],
        );
        lbl(
            self,
            "Game mouse / capture mode (hotkey)",
            [476, 186, 380, 18],
        );
        self.add(
            "msctls_hotkey32",
            "",
            WS_TABSTOP,
            WS_EX_CLIENTEDGE,
            ID_HK_CAPTURE,
            [476, 206, 220, 24],
        );
        lbl(self, "Click a box and press the new combination. Hotkeys are paused while this window is focused.", [476, 234, 384, 34]);
        self.add(
            "BUTTON",
            "Ignore software-injected mouse movement",
            chk,
            0,
            ID_IGNINJ,
            [476, 274, 384, 22],
        );
        self.add(
            "BUTTON",
            "Start with Windows",
            chk,
            0,
            ID_AUTOSTART,
            [476, 302, 384, 22],
        );

        group(self, "About", [464, 360, 404, 170]);
        self.add(
            "STATIC",
            &format!("glidekm {}", env!("CARGO_PKG_VERSION")),
            SS_LEFT,
            0,
            ID_VERSION,
            [476, 384, 380, 18],
        );
        self.add(
            "BUTTON",
            "Open config folder",
            btn,
            0,
            ID_OPEN_CFG,
            [476, 490, 170, 26],
        );
        self.add(
            "BUTTON",
            "Open log",
            btn,
            0,
            ID_OPEN_LOG,
            [652, 490, 110, 26],
        );

        self.add(
            "STATIC",
            "",
            SS_LEFT | SS_ENDELLIPSIS,
            0,
            ID_MSG,
            [16, 548, 640, 22],
        );
        self.add("BUTTON", "Revert", btn, 0, ID_REVERT, [676, 543, 92, 28]);
        self.add(
            "BUTTON",
            "Save",
            btn | BS_DEFPUSHBUTTON as u32,
            0,
            ID_SAVE,
            [776, 543, 92, 28],
        );
    }

    /// Fonts and positions for the current DPI.
    fn relayout(&mut self) {
        unsafe {
            if !self.font.is_null() {
                DeleteObject(self.font as _);
                DeleteObject(self.head_font as _);
            }
            let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
            ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
            SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                &mut ncm as *mut _ as _,
                0,
                self.dpi,
            );
            self.font = CreateFontIndirectW(&ncm.lfMessageFont);
            let mut head = ncm.lfMessageFont;
            head.lfWeight = 600;
            head.lfHeight = (head.lfHeight as f64 * 1.15) as i32;
            self.head_font = CreateFontIndirectW(&head);
            for (h, r) in &self.ctrls {
                MoveWindow(
                    *h,
                    self.s(r[0]),
                    self.s(r[1]),
                    self.s(r[2]),
                    self.s(r[3]),
                    1,
                );
                SendMessageW(*h, WM_SETFONT, self.font as usize, 1);
            }
        }
    }

    // ------------------------------------------------------------ painting

    fn paint_header(&self, hdc: HDC) {
        let (st, text, capture) = &self.status;
        let color = match st {
            Status::Unconfigured => rgb(138, 145, 156),
            Status::Connecting => rgb(217, 119, 6),
            Status::Online => rgb(22, 163, 74),
            Status::Active => rgb(37, 99, 235),
        };
        unsafe {
            let old_font = SelectObject(hdc, self.head_font as _);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, GetSysColor(COLOR_WINDOWTEXT));
            let title = wide("glidekm");
            let mut r = RECT {
                left: self.s(16),
                top: 0,
                right: self.s(120),
                bottom: self.s(HEADER_H),
            };
            DrawTextW(hdc, title.as_ptr(), -1, &mut r, DT_SINGLELINE | DT_VCENTER);
            SelectObject(hdc, self.font as _);
            let brush = CreateSolidBrush(color);
            let pen = CreatePen(PS_SOLID, 1, color);
            let ob = SelectObject(hdc, brush as _);
            let op = SelectObject(hdc, pen as _);
            let cy = self.s(HEADER_H) / 2;
            Ellipse(
                hdc,
                self.s(104),
                cy - self.s(5),
                self.s(114),
                cy + self.s(5),
            );
            SelectObject(hdc, ob);
            SelectObject(hdc, op);
            DeleteObject(brush as _);
            DeleteObject(pen as _);
            let t = wide(&format!(
                "{text}{}",
                if *st == Status::Active && *capture {
                    " · game mouse"
                } else {
                    ""
                }
            ));
            let mut r = RECT {
                left: self.s(122),
                top: 0,
                right: self.s(730),
                bottom: self.s(HEADER_H),
            };
            DrawTextW(
                hdc,
                t.as_ptr(),
                -1,
                &mut r,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
            let line = CreatePen(PS_SOLID, 1, GetSysColor(COLOR_3DLIGHT));
            let op = SelectObject(hdc, line as _);
            MoveToEx(hdc, 0, self.s(HEADER_H), std::ptr::null_mut());
            LineTo(hdc, self.s(CLIENT_W) + 2000, self.s(HEADER_H));
            SelectObject(hdc, op);
            DeleteObject(line as _);
            SelectObject(hdc, old_font);
        }
    }

    fn paint_panel(&mut self, hdc: HDC, w: i32, h: i32) {
        let l = self.layout_for();
        let d = l.desktop();
        let (cx, cy, cw, ch) = l.client_rect();
        // Room for the Deck on every side, so the view doesn't jump while dragging.
        let (padx, pady) = (cw * 1.06, ch * 1.06);
        let (vx, vy) = (d.l as f64 - padx, d.t as f64 - pady);
        let (vw, vh) = (
            (d.r - d.l) as f64 + 2.0 * padx,
            (d.b - d.t) as f64 + 2.0 * pady,
        );
        let k = (w as f64 / vw).min(h as f64 / vh);
        let ox = (w as f64 - vw * k) / 2.0 - vx * k;
        let oy = (h as f64 - vh * k) / 2.0 - vy * k;
        self.view = View { k, ox, oy };
        let px = |x: f64| (x * k + ox).round() as i32;
        let py = |y: f64| (y * k + oy).round() as i32;
        unsafe {
            let bg = CreateSolidBrush(rgb(243, 244, 246));
            let full = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            FillRect(hdc, &full, bg);
            DeleteObject(bg as _);
            SetBkMode(hdc, TRANSPARENT as i32);
            let old_font = SelectObject(hdc, self.font as _);
            let rad = self.s(6);
            let shape = |hdc: HDC, r: [i32; 4], fill: COLORREF, line: COLORREF, width: i32| {
                let b = CreateSolidBrush(fill);
                let p = CreatePen(PS_SOLID, width, line);
                let ob = SelectObject(hdc, b as _);
                let op = SelectObject(hdc, p as _);
                RoundRect(hdc, r[0], r[1], r[2], r[3], rad, rad);
                SelectObject(hdc, ob);
                SelectObject(hdc, op);
                DeleteObject(b as _);
                DeleteObject(p as _);
            };
            let text2 = |hdc: HDC, r: [i32; 4], a: &str, b: &str, color: COLORREF| {
                SetTextColor(hdc, color);
                let mid = (r[1] + r[3]) / 2;
                let wa = wide(a);
                let mut ra = RECT {
                    left: r[0],
                    top: r[1],
                    right: r[2],
                    bottom: mid,
                };
                DrawTextW(
                    hdc,
                    wa.as_ptr(),
                    -1,
                    &mut ra,
                    DT_SINGLELINE | DT_CENTER | DT_BOTTOM | DT_END_ELLIPSIS,
                );
                let wb = wide(b);
                let mut rb = RECT {
                    left: r[0],
                    top: mid,
                    right: r[2],
                    bottom: r[3],
                };
                DrawTextW(
                    hdc,
                    wb.as_ptr(),
                    -1,
                    &mut rb,
                    DT_SINGLELINE | DT_CENTER | DT_TOP | DT_END_ELLIPSIS,
                );
            };
            for (i, m) in l.monitors.iter().enumerate() {
                let r = [
                    px(m.l as f64) + 1,
                    py(m.t as f64) + 1,
                    px(m.r as f64) - 1,
                    py(m.b as f64) - 1,
                ];
                shape(
                    hdc,
                    r,
                    rgb(229, 231, 235),
                    rgb(156, 163, 175),
                    self.s(1).max(1),
                );
                text2(
                    hdc,
                    r,
                    &format!("{}", i + 1),
                    &format!("{}×{}", m.r - m.l, m.b - m.t),
                    rgb(55, 65, 81),
                );
            }
            let dr = [px(cx), py(cy), px(cx + cw), py(cy + ch)];
            let active = self.status.0 == Status::Active;
            shape(
                hdc,
                dr,
                if active {
                    rgb(191, 219, 254)
                } else {
                    rgb(219, 234, 254)
                },
                rgb(37, 99, 235),
                self.s(2),
            );
            let (dw, dh) = self.ctl.shared.size().unwrap_or((1280, 800));
            text2(
                hdc,
                dr,
                "Steam Deck",
                &format!("{dw}×{dh}"),
                rgb(30, 58, 138),
            );
            let green = CreateSolidBrush(rgb(22, 163, 74));
            let t = self.s(2);
            for (lo, hi) in l.shared_segments() {
                let r = match l.side {
                    Side::Right => RECT {
                        left: px(d.r as f64) - t,
                        top: py(lo),
                        right: px(d.r as f64) + t,
                        bottom: py(hi),
                    },
                    Side::Left => RECT {
                        left: px(d.l as f64) - t,
                        top: py(lo),
                        right: px(d.l as f64) + t,
                        bottom: py(hi),
                    },
                    Side::Bottom => RECT {
                        left: px(lo),
                        top: py(d.b as f64) - t,
                        right: px(hi),
                        bottom: py(d.b as f64) + t,
                    },
                    Side::Top => RECT {
                        left: px(lo),
                        top: py(d.t as f64) - t,
                        right: px(hi),
                        bottom: py(d.t as f64) + t,
                    },
                };
                FillRect(hdc, &r, green);
            }
            DeleteObject(green as _);
            SelectObject(hdc, old_font);
        }
    }

    fn deck_hit(&self, x: i32, y: i32) -> bool {
        let l = self.layout_for();
        let (cx, cy, cw, ch) = l.client_rect();
        let v = &self.view;
        let (dx, dy) = ((x as f64 - v.ox) / v.k, (y as f64 - v.oy) / v.k);
        dx >= cx && dx <= cx + cw && dy >= cy && dy <= cy + ch
    }

    fn drag_to(&mut self, x: i32, y: i32) {
        let Some((gx, gy)) = self.drag else { return };
        let v = &self.view;
        let (dx, dy) = ((x as f64 - v.ox) / v.k - gx, (y as f64 - v.oy) / v.k - gy);
        let magnet = 10.0 / v.k;
        let (side, off) = self.layout_for().snap(dx, dy, magnet);
        if (side, off) != (self.draft.side, self.draft.offset) {
            self.draft.side = side;
            self.draft.offset = off;
            self.updating = true;
            let idx = [Side::Right, Side::Left, Side::Top, Side::Bottom]
                .iter()
                .position(|s| *s == side)
                .unwrap();
            unsafe { SendMessageW(self.item(ID_SIDE), CB_SETCURSEL, idx, 0) };
            self.set_text(ID_OFFSET, &off.to_string());
            self.updating = false;
            self.after_change();
        }
    }
}

// ---------------------------------------------------------------- hotkey control

const HKF_SHIFT: u8 = 1;
const HKF_CONTROL: u8 = 2;
const HKF_ALT: u8 = 4;

unsafe fn set_hotkey(h: HWND, s: &str) {
    if let Ok(hk) = Hotkey::parse(s) {
        let mut f = 0u8;
        if hk.mods & MOD_SHIFT != 0 {
            f |= HKF_SHIFT;
        }
        if hk.mods & MOD_CTRL != 0 {
            f |= HKF_CONTROL;
        }
        if hk.mods & MOD_ALT != 0 {
            f |= HKF_ALT;
        }
        SendMessageW(
            h,
            HKM_SETHOTKEY,
            (hk.vk & 0xFF) as usize | (f as usize) << 8,
            0,
        );
    }
}

/// Current value as a config string; None if empty or not representable.
fn get_hotkey(h: HWND) -> Option<String> {
    let v = unsafe { SendMessageW(h, HKM_GETHOTKEY, 0, 0) } as u32;
    let (vk, f) = (v & 0xFF, ((v >> 8) & 0xFF) as u8);
    if vk == 0 {
        return None;
    }
    let mut mods = 0;
    if f & HKF_SHIFT != 0 {
        mods |= MOD_SHIFT;
    }
    if f & HKF_CONTROL != 0 {
        mods |= MOD_CTRL;
    }
    if f & HKF_ALT != 0 {
        mods |= MOD_ALT;
    }
    let _ = MOD_WIN;
    Hotkey { mods, vk }.to_config()
}

fn generate_passphrase() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    glidekm_core::session::random_bytes()
        .iter()
        .take(20)
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

// ---------------------------------------------------------------- window procs

fn bring_up(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
    }
    // Pick up changes made elsewhere (e.g. config edited by hand) if clean.
    with_ui(|u| {
        if !u.dirty() {
            u.saved = Draft::from(&u.ctl.cfg.lock().unwrap());
            u.draft = u.saved.clone();
            u.fill();
        }
        u.tick();
    });
}

fn hide(hwnd: HWND) {
    unsafe { ShowWindow(hwnd, SW_HIDE) };
    PAUSE_HOTKEYS.store(false, Ordering::SeqCst);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let (id, code) = ((wp & 0xFFFF) as u16, ((wp >> 16) & 0xFFFF) as u32);
            with_ui(|u| u.on_command(id, code));
            0
        }
        WM_HSCROLL => {
            with_ui(|u| {
                if lp as HWND == u.item(ID_SPEED) {
                    let v = SendMessageW(u.item(ID_SPEED), TBM_GETPOS, 0, 0) as u32;
                    u.draft.scale_pct = (v / 5 * 5).clamp(50, 300);
                    u.set_text(
                        ID_SPEEDVAL,
                        &format!("{:.2}", u.draft.scale_pct as f64 / 100.0),
                    );
                    u.after_change();
                }
            });
            0
        }
        WM_TIMER => {
            with_ui(|u| u.tick());
            0
        }
        WM_APP_SHOW => {
            bring_up(hwnd);
            0
        }
        WM_APP_SET_ADDR => {
            with_ui(|u| {
                u.updating = true;
                let a = u.draft.client.clone();
                u.set_text(ID_ADDR, &a);
                u.updating = false;
                u.after_change();
            });
            0
        }
        WM_APP_SCAN_DONE => {
            with_ui(|u| u.on_scan_done());
            0
        }
        WM_ACTIVATE => {
            PAUSE_HOTKEYS.store((wp & 0xFFFF) as u32 != WA_INACTIVE, Ordering::SeqCst);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_SYSCOMMAND if (wp & 0xFFF0) as u32 == SC_MINIMIZE => {
            // Minimize = go to the tray: no taskbar button, background only.
            hide(hwnd);
            0
        }
        WM_CLOSE => {
            let dirty = with_ui(|u| u.dirty()).unwrap_or(false);
            if dirty {
                let t = wide("Save your changes to the glidekm settings?");
                let c = wide("glidekm");
                match MessageBoxW(
                    hwnd,
                    t.as_ptr(),
                    c.as_ptr(),
                    MB_YESNOCANCEL | MB_ICONQUESTION,
                ) {
                    IDYES => {
                        with_ui(|u| u.save());
                        if with_ui(|u| u.dirty()).unwrap_or(false) {
                            return 0; // save failed: keep the window open
                        }
                    }
                    IDNO => {
                        with_ui(|u| {
                            u.draft = u.saved.clone();
                            u.set_msg("", MsgKind::Info);
                            u.fill();
                        });
                    }
                    _ => return 0,
                }
            }
            hide(hwnd);
            0
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_ui(|u| u.paint_header(hdc));
            EndPaint(hwnd, &ps);
            0
        }
        WM_CTLCOLORSTATIC => {
            let hdc = wp as HDC;
            SetBkColor(hdc, GetSysColor(COLOR_WINDOW));
            let kind = with_ui(|u| (lp as HWND == u.item(ID_MSG)).then_some(u.msg.1)).flatten();
            match kind {
                Some(MsgKind::Ok) => SetTextColor(hdc, rgb(22, 128, 61)),
                Some(MsgKind::Err) => SetTextColor(hdc, rgb(200, 30, 30)),
                _ => SetTextColor(hdc, GetSysColor(COLOR_WINDOWTEXT)),
            };
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }
        WM_DPICHANGED => {
            let r = &*(lp as *const RECT);
            with_ui(|u| {
                u.dpi = (wp & 0xFFFF) as u32;
                u.relayout();
            });
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            InvalidateRect(hwnd, std::ptr::null(), 1);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let x = (lp & 0xFFFF) as i16 as i32;
    let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            // Double buffer to avoid flicker while dragging.
            let mem = CreateCompatibleDC(hdc);
            let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
            let old = SelectObject(mem, bmp as _);
            with_ui(|u| u.paint_panel(mem, rc.right, rc.bottom));
            BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old);
            DeleteObject(bmp as _);
            DeleteDC(mem);
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => {
            with_ui(|u| {
                if u.deck_hit(x, y) {
                    let l = u.layout_for();
                    let (cx, cy, _, _) = l.client_rect();
                    let v = &u.view;
                    u.drag = Some(((x as f64 - v.ox) / v.k - cx, (y as f64 - v.oy) / v.k - cy));
                    SetCapture(hwnd);
                }
            });
            0
        }
        WM_MOUSEMOVE => {
            with_ui(|u| u.drag_to(x, y));
            0
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            with_ui(|u| u.drag = None);
            if msg == WM_LBUTTONUP {
                ReleaseCapture();
            }
            0
        }
        WM_SETCURSOR => {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            ScreenToClient(hwnd, &mut pt);
            let over = with_ui(|u| u.drag.is_some() || u.deck_hit(pt.x, pt.y)).unwrap_or(false);
            SetCursor(LoadCursorW(
                std::ptr::null_mut(),
                if over { IDC_SIZEALL } else { IDC_ARROW },
            ));
            1
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn run(ctl: Arc<Control>) {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES | ICC_HOTKEY_CLASS | ICC_WIN95_CLASSES,
    };
    InitCommonControlsEx(&icc);
    let hinst = GetModuleHandleW(std::ptr::null());
    let class = wide(CLASS);
    let panel_class = wide(PANEL_CLASS);
    let icon = LoadIconW(hinst, 1 as _);

    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(wndproc);
    wc.hInstance = hinst;
    wc.lpszClassName = class.as_ptr();
    wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
    wc.hbrBackground = (COLOR_WINDOW + 1) as usize as HBRUSH;
    wc.hIcon = icon;
    RegisterClassW(&wc);
    let mut pc: WNDCLASSW = std::mem::zeroed();
    pc.lpfnWndProc = Some(panel_proc);
    pc.hInstance = hinst;
    pc.lpszClassName = panel_class.as_ptr();
    pc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
    RegisterClassW(&pc);

    // Size the window for the DPI of the monitor under the cursor.
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
    let (mut dx, mut dy) = (96u32, 96u32);
    GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let mut r = RECT {
        left: 0,
        top: 0,
        right: mul_div(CLIENT_W, dx as i32, 96),
        bottom: mul_div(CLIENT_H, dx as i32, 96),
    };
    AdjustWindowRectExForDpi(&mut r, style, 0, WS_EX_CONTROLPARENT, dx);
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(mon, &mut mi);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let x = mi.rcWork.left + ((mi.rcWork.right - mi.rcWork.left) - w).max(0) / 2;
    let y = mi.rcWork.top + ((mi.rcWork.bottom - mi.rcWork.top) - h).max(0) / 2;
    let title = wide("glidekm settings");
    let hwnd = CreateWindowExW(
        WS_EX_CONTROLPARENT,
        class.as_ptr(),
        title.as_ptr(),
        style,
        x,
        y,
        w,
        h,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        glidekm_core::warn!("cannot create settings window");
        return;
    }
    WINDOW.store(hwnd as isize, Ordering::SeqCst);
    let saved = Draft::from(&ctl.cfg.lock().unwrap());
    let ui = Ui {
        ctl,
        hwnd,
        panel: std::ptr::null_mut(),
        dpi: GetDpiForWindow(hwnd),
        font: std::ptr::null_mut(),
        head_font: std::ptr::null_mut(),
        ctrls: Vec::new(),
        draft: saved.clone(),
        saved,
        updating: false,
        drag: None,
        view: View {
            k: 1.0,
            ox: 0.0,
            oy: 0.0,
        },
        status: (Status::Unconfigured, String::new(), false),
        msg: (String::new(), MsgKind::Info),
        show_pass: false,
    };
    UI.with(|u| *u.borrow_mut() = Some(ui));
    with_ui(|u| {
        u.build();
        u.relayout();
        u.fill();
        if !u.ctl.cfg.lock().unwrap().configured() {
            u.set_msg(
                "Enter the Deck's address (or Scan) and a passphrase, then Save.",
                MsgKind::Info,
            );
        }
        u.tick();
    });
    SetTimer(hwnd, TIMER_REFRESH, 500, None);
    bring_up(hwnd);
    with_ui(|u| {
        if u.ctl.cfg.lock().unwrap().client.is_empty() {
            SetFocus(u.item(ID_ADDR));
        }
    });

    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
        // Tab / Enter navigation between controls.
        if IsDialogMessageW(hwnd, &msg) == 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
