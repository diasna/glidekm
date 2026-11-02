//! Windows adapter: low-level hooks + Raw Input feed the engine; a hidden
//! window runs deferred cursor actions, timers and display-change handling.

use crate::engine::{Action, Engine, Input};
use crate::layout::{Layout, Rect};
use crate::{net, Settings};
use glidekm_core::{info, warn};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_ACTIONS: u32 = WM_APP + 1;
const TIMER_TICK: usize = 1;

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    static HWND_MAIN: std::cell::Cell<HWND> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
/// Treat software-injected mouse moves as non-physical (config `ignore_injected`).
static IGNORE_INJECTED: AtomicBool = AtomicBool::new(false);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------- cursor

const CURSOR_IDS: [u32; 16] = [
    32512, 32513, 32514, 32515, 32516, 32640, 32641, 32642, 32643, 32644, 32645, 32646, 32648,
    32649, 32650, 32651,
];

fn hide_cursor() {
    if CURSOR_HIDDEN.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let and_mask = [0xFFu8; 128];
        let xor_mask = [0u8; 128];
        for id in CURSOR_IDS {
            // SetSystemCursor takes ownership, so create one blank cursor per id.
            let c = CreateCursor(
                hinst,
                0,
                0,
                32,
                32,
                and_mask.as_ptr() as _,
                xor_mask.as_ptr() as _,
            );
            if !c.is_null() {
                SetSystemCursor(c, id as _);
            }
        }
    }
}

pub fn restore_cursor() {
    CURSOR_HIDDEN.store(false, Ordering::SeqCst);
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0);
    }
}

unsafe extern "system" fn console_handler(_ctrl: u32) -> BOOL {
    restore_cursor();
    0 // let the default handler terminate the process
}

// ---------------------------------------------------------------- monitors

unsafe extern "system" fn monitor_cb(
    hmon: HMONITOR,
    _hdc: HDC,
    _r: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let v = &mut *(data as *mut Vec<Rect>);
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(hmon, &mut mi) != 0 {
        let r = mi.rcMonitor;
        v.push(Rect {
            l: r.left,
            t: r.top,
            r: r.right,
            b: r.bottom,
        });
    }
    1
}

fn monitors() -> Vec<Rect> {
    let mut v: Vec<Rect> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(monitor_cb),
            &mut v as *mut _ as LPARAM,
        );
    }
    v
}

// ---------------------------------------------------------------- engine glue

/// Feed the engine; returns whether to block. Never panics on re-entrancy.
fn feed(inp: Input) -> bool {
    ENGINE.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(e) = g.as_mut() else { return false };
        let block = e.input(inp);
        if !e.actions.is_empty() {
            unsafe {
                PostMessageW(HWND_MAIN.with(|h| h.get()), WM_APP_ACTIONS, 0, 0);
            }
        }
        block
    })
}

fn run_actions() {
    let acts: Vec<Action> = ENGINE.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut g| g.as_mut().map(|e| std::mem::take(&mut e.actions)))
            .unwrap_or_default()
    });
    for a in acts {
        match a {
            Action::HideCursor => hide_cursor(),
            Action::ShowCursor => restore_cursor(),
            Action::Warp(x, y) => unsafe {
                SetCursorPos(x, y);
            },
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let ms = &*(lparam as *const MSLLHOOKSTRUCT);
        let injected = ms.flags & LLMHF_INJECTED != 0 && IGNORE_INJECTED.load(Ordering::Relaxed);
        let hi = (ms.mouseData >> 16) as u16;
        let inp = match wparam as u32 {
            WM_MOUSEMOVE => {
                let mut old = POINT { x: 0, y: 0 };
                GetCursorPos(&mut old);
                Some(Input::Move {
                    old: (old.x, old.y),
                    new: (ms.pt.x, ms.pt.y),
                    injected,
                })
            }
            WM_LBUTTONDOWN => Some(Input::Button { idx: 0, down: true }),
            WM_LBUTTONUP => Some(Input::Button {
                idx: 0,
                down: false,
            }),
            WM_RBUTTONDOWN => Some(Input::Button { idx: 1, down: true }),
            WM_RBUTTONUP => Some(Input::Button {
                idx: 1,
                down: false,
            }),
            WM_MBUTTONDOWN => Some(Input::Button { idx: 2, down: true }),
            WM_MBUTTONUP => Some(Input::Button {
                idx: 2,
                down: false,
            }),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let idx = if hi == XBUTTON2 { 4 } else { 3 };
                Some(Input::Button {
                    idx,
                    down: wparam as u32 == WM_XBUTTONDOWN,
                })
            }
            WM_MOUSEWHEEL => Some(Input::Wheel {
                horizontal: false,
                delta: hi as i16,
            }),
            WM_MOUSEHWHEEL => Some(Input::Wheel {
                horizontal: true,
                delta: hi as i16,
            }),
            _ => None,
        };
        if let Some(inp) = inp {
            if feed(inp) {
                return 1;
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn kbd_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        let inp = Input::Key {
            vk: kb.vkCode,
            scan: kb.scanCode,
            extended: kb.flags & LLKHF_EXTENDED != 0,
            down: kb.flags & LLKHF_UP == 0,
        };
        let _ = wparam;
        if feed(inp) {
            return 1;
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe fn on_raw_input(lparam: LPARAM) {
    let mut buf = [0u64; 8]; // RAWINPUT for a mouse is 48 bytes; 8-byte aligned
    let mut size = std::mem::size_of_val(&buf) as u32;
    let n = GetRawInputData(
        lparam as HRAWINPUT,
        RID_INPUT,
        buf.as_mut_ptr() as *mut _,
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    if n == u32::MAX || n == 0 {
        return;
    }
    let ri = &*(buf.as_ptr() as *const RAWINPUT);
    if ri.header.dwType == RIM_TYPEMOUSE {
        let m = &ri.data.mouse;
        if m.usFlags & MOUSE_MOVE_ABSOLUTE == 0 && (m.lLastX != 0 || m.lLastY != 0) {
            feed(Input::Raw {
                dx: m.lLastX,
                dy: m.lLastY,
            });
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_ACTIONS => {
            run_actions();
            0
        }
        WM_INPUT => {
            on_raw_input(lparam);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER => {
            feed(Input::Tick);
            run_actions();
            0
        }
        WM_DISPLAYCHANGE => {
            let m = monitors();
            info!("display layout changed: {} monitor(s)", m.len());
            ENGINE.with(|cell| {
                if let Ok(mut g) = cell.try_borrow_mut() {
                    if let Some(e) = g.as_mut() {
                        e.layout.monitors = m;
                    }
                }
            });
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------- entry

pub fn run(s: Settings) -> Result<(), String> {
    unsafe {
        if s.background {
            FreeConsole();
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
        // A previous crash may have left the blank cursor installed.
        restore_cursor();
        SetConsoleCtrlHandler(Some(console_handler), 1);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        restore_cursor();
        prev(p)
    }));

    IGNORE_INJECTED.store(s.cfg.flag("ignore_injected"), Ordering::Relaxed);
    let shared = net::Shared::new();
    let tx =
        net::spawn(s.client.clone(), s.psk, shared.clone()).map_err(|e| format!("network: {e}"))?;
    let mons = monitors();
    if mons.is_empty() {
        return Err("no monitors found".into());
    }
    let layout = Layout {
        monitors: mons,
        side: s.side,
        offset: s.offset,
        scale: s.scale,
        client_w: 0,
        client_h: 0,
    };
    info!(
        "glidekm-server {} | client {} on the {:?} | desktop {:?}",
        env!("CARGO_PKG_VERSION"),
        s.client,
        s.side,
        layout.desktop()
    );
    info!(
        "hotkeys: switch = {:?}, mouse mode = {:?}",
        s.cfg.str_or("hotkey_switch", "ctrl+alt+shift+s"),
        s.cfg.str_or("hotkey_capture", "ctrl+alt+shift+m")
    );
    ENGINE.with(|c| {
        *c.borrow_mut() = Some(Engine::new(layout, tx, shared, s.hk_switch, s.hk_capture))
    });

    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("glidekm-server");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        // A real (never shown) top-level window: message-only windows do not
        // receive broadcasts such as WM_DISPLAYCHANGE.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err("CreateWindowExW failed".into());
        }
        HWND_MAIN.with(|h| h.set(hwnd));

        let rid = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        if RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            warn!("raw input unavailable; game mode will use accelerated deltas");
        }

        let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0);
        let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), hinst, 0);
        if mh.is_null() || kh.is_null() {
            return Err("failed to install input hooks".into());
        }
        SetTimer(hwnd, TIMER_TICK, 100, None);
        info!(
            "hooks installed; move the cursor past the {:?} edge to switch",
            s.side
        );

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(mh);
        UnhookWindowsHookEx(kh);
    }
    restore_cursor();
    Ok(())
}
