//! Platform-independent capture state machine. The Windows hook adapter turns
//! OS events into `Input`s; the engine decides whether to block them, what to
//! send to the client, and which cursor `Action`s to perform afterwards.

use crate::hotkey::{vk_modifier, Hotkey};
use crate::layout::{Layout, Step};
use crate::net::Shared;
use glidekm_core::event::{bit_set, Event};
use glidekm_core::keymap::{win_to_evdev, MOUSE_BUTTONS};
use glidekm_core::{debug, info};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    HideCursor,
    ShowCursor,
    Warp(i32, i32),
}

#[derive(Debug, Clone, Copy)]
pub enum Input {
    /// `old` = cursor position before this move, `new` = hook-reported target.
    Move {
        old: (i32, i32),
        new: (i32, i32),
        injected: bool,
    },
    /// Unaccelerated device counts (Raw Input).
    Raw {
        dx: i32,
        dy: i32,
    },
    /// idx: 0 left, 1 right, 2 middle, 3 back (X1), 4 forward (X2)
    Button {
        idx: usize,
        down: bool,
    },
    Wheel {
        horizontal: bool,
        delta: i16,
    },
    Key {
        vk: u32,
        scan: u32,
        extended: bool,
        down: bool,
    },
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Local,
    Remote { pos: (f64, f64), capture: bool },
}

pub struct Engine {
    pub layout: Layout,
    mode: Mode,
    tx: Sender<Event>,
    shared: Arc<Shared>,
    hk_switch: Hotkey,
    hk_capture: Hotkey,
    /// Physical key state by virtual key (both modes), for modifiers.
    vk_down: [bool; 256],
    /// Keys/buttons pressed while local: their release must reach Windows.
    local_keys: [bool; 256],
    local_buttons: [bool; 5],
    /// Keys whose release must be swallowed (hotkey keys).
    swallow: [bool; 256],
    remote_keys: Vec<u16>,
    last_raw: Option<Instant>,
    sent_pos: (u16, u16),
    pub actions: Vec<Action>,
}

impl Engine {
    pub fn new(
        layout: Layout,
        tx: Sender<Event>,
        shared: Arc<Shared>,
        hk_switch: Hotkey,
        hk_capture: Hotkey,
    ) -> Self {
        Engine {
            layout,
            mode: Mode::Local,
            tx,
            shared,
            hk_switch,
            hk_capture,
            vk_down: [false; 256],
            local_keys: [false; 256],
            local_buttons: [false; 5],
            swallow: [false; 256],
            remote_keys: Vec::new(),
            last_raw: None,
            sent_pos: (u16::MAX, u16::MAX),
            actions: Vec::new(),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.mode, Mode::Remote { .. })
    }

    fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    fn mods(&self) -> u8 {
        self.vk_down
            .iter()
            .enumerate()
            .filter(|(_, d)| **d)
            .fold(0, |m, (vk, _)| m | vk_modifier(vk as u32))
    }

    fn sync_size(&mut self) -> bool {
        match self.shared.size() {
            Some((w, h)) => {
                if (w, h) != (self.layout.client_w, self.layout.client_h) {
                    self.layout.client_w = w;
                    self.layout.client_h = h;
                    if let Mode::Remote { pos, capture } = self.mode {
                        self.mode = Mode::Remote {
                            pos: self.layout.clamp(pos),
                            capture,
                        };
                    }
                }
                true
            }
            None => false,
        }
    }

    fn set_remote_key(&mut self, code: u16, down: bool) {
        if down {
            self.remote_keys.push(code);
        } else {
            self.remote_keys.retain(|&c| c != code);
        }
        bit_set(&mut self.shared.pressed.lock().unwrap(), code, down);
    }

    /// `near` = a Windows point used to pick which monitor to park the hidden
    /// cursor on.
    fn enter(&mut self, pos: (f64, f64), near: Option<(i32, i32)>) -> bool {
        if !self.shared.alive.load(Ordering::Relaxed) || !self.sync_size() {
            return false;
        }
        let pos = self.layout.clamp(pos);
        self.mode = Mode::Remote {
            pos,
            capture: false,
        };
        self.sent_pos = (pos.0.round() as u16, pos.1.round() as u16);
        self.remote_keys.clear();
        *self.shared.pressed.lock().unwrap() = [0; glidekm_core::event::KEY_BITMAP_LEN];
        self.shared.capture.store(false, Ordering::Relaxed);
        self.shared.active.store(true, Ordering::Relaxed);
        self.send(Event::Enter {
            x: self.sent_pos.0,
            y: self.sent_pos.1,
            capture: false,
        });
        self.actions.push(Action::HideCursor);
        // Park the (hidden) cursor in the middle of a monitor so hook deltas
        // are never clamped by a screen edge.
        let park = self.layout.park_point(near);
        self.actions.push(Action::Warp(park.0, park.1));
        info!("-> client at ({}, {})", self.sent_pos.0, self.sent_pos.1);
        true
    }

    fn leave(&mut self, warp: Option<(i32, i32)>) {
        self.mode = Mode::Local;
        self.shared.active.store(false, Ordering::Relaxed);
        self.shared.capture.store(false, Ordering::Relaxed);
        self.remote_keys.clear();
        *self.shared.pressed.lock().unwrap() = [0; glidekm_core::event::KEY_BITMAP_LEN];
        self.send(Event::Leave);
        if let Some((x, y)) = warp {
            self.actions.push(Action::Warp(x, y));
        }
        self.actions.push(Action::ShowCursor);
        info!("<- local");
    }

    /// Switch PC <-> Deck (hotkey or tray menu). Enters at the Deck's center.
    pub fn toggle(&mut self) {
        if self.is_remote() {
            self.leave_at_current();
        } else {
            let (w, h) = self.shared.size().unwrap_or((0, 0));
            if !self.enter((w as f64 / 2.0, h as f64 / 2.0), None) {
                info!("switch ignored: client not connected");
            }
        }
    }

    /// Apply new settings from the UI. Leaves the Deck first if the
    /// arrangement changed while it is in use.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn reconfigure(&mut self, p: crate::control::EngineParams) {
        let moved = p.side != self.layout.side
            || p.offset != self.layout.offset
            || p.scale != self.layout.scale;
        if moved && self.is_remote() {
            self.leave_at_current();
        }
        self.layout.side = p.side;
        self.layout.offset = p.offset;
        self.layout.scale = p.scale;
        self.hk_switch = p.hk_switch;
        self.hk_capture = p.hk_capture;
    }

    fn leave_at_current(&mut self) {
        if let Mode::Remote { pos, .. } = self.mode {
            let p = self.layout.exit_point_for(pos);
            self.leave(Some(p));
        }
    }

    fn raw_recent(&self) -> bool {
        self.last_raw
            .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
    }

    /// Returns true if the event must be blocked from Windows.
    pub fn input(&mut self, inp: Input) -> bool {
        match inp {
            Input::Tick => {
                if self.is_remote() && !self.shared.alive.load(Ordering::Relaxed) {
                    info!("client lost while active");
                    self.leave_at_current();
                }
                self.sync_size();
                false
            }
            Input::Raw { dx, dy } => {
                self.last_raw = Some(Instant::now());
                if let Mode::Remote { capture: true, .. } = self.mode {
                    self.send_motion(dx, dy);
                }
                false
            }
            Input::Move { old, new, injected } => self.on_move(old, new, injected),
            Input::Button { idx, down } => self.on_button(idx, down),
            Input::Wheel { horizontal, delta } => {
                if self.is_remote() {
                    self.send(Event::Wheel {
                        horizontal,
                        v120: delta,
                    });
                    true
                } else {
                    false
                }
            }
            Input::Key {
                vk,
                scan,
                extended,
                down,
            } => self.on_key(vk, scan, extended, down),
        }
    }

    fn send_motion(&self, dx: i32, dy: i32) {
        if dx != 0 || dy != 0 {
            let c = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            self.send(Event::Motion {
                dx: c(dx),
                dy: c(dy),
            });
        }
    }

    fn on_move(&mut self, old: (i32, i32), new: (i32, i32), injected: bool) -> bool {
        match self.mode {
            Mode::Local => {
                if injected || self.local_buttons.iter().any(|b| *b) {
                    return false; // don't switch while dragging
                }
                if !self.shared.alive.load(Ordering::Relaxed) {
                    return false;
                }
                self.sync_size();
                if let Some(p) = self.layout.try_enter(old, new) {
                    return self.enter(p, Some(old)); // block the crossing move
                }
                false
            }
            Mode::Remote { pos, capture } => {
                if injected {
                    return true;
                }
                let (dx, dy) = (new.0 - old.0, new.1 - old.1);
                if capture {
                    if !self.raw_recent() {
                        self.send_motion(dx, dy);
                    }
                    return true;
                }
                match self.layout.step(pos, (dx as f64, dy as f64)) {
                    Step::Stay(x, y) => {
                        self.mode = Mode::Remote {
                            pos: (x, y),
                            capture,
                        };
                        let p = (x.round() as u16, y.round() as u16);
                        if p != self.sent_pos {
                            self.sent_pos = p;
                            self.send(Event::MoveAbs { x: p.0, y: p.1 });
                        }
                    }
                    Step::Exit(x, y) => self.leave(Some((x, y))),
                }
                true
            }
        }
    }

    fn on_button(&mut self, idx: usize, down: bool) -> bool {
        if idx >= 5 {
            return false;
        }
        if !self.is_remote() {
            self.local_buttons[idx] = down;
            return false;
        }
        if !down && self.local_buttons[idx] {
            self.local_buttons[idx] = false;
            return false; // pressed before switching: release on Windows
        }
        let code = MOUSE_BUTTONS[idx];
        if down != self.remote_keys.contains(&code) {
            self.set_remote_key(code, down);
            self.send(Event::Button { code, down });
        }
        true
    }

    fn on_key(&mut self, vk: u32, scan: u32, extended: bool, down: bool) -> bool {
        let vki = (vk & 0xff) as usize;
        let mods_before = self.mods();
        self.vk_down[vki] = down;

        if !down && self.swallow[vki] {
            self.swallow[vki] = false;
            return true;
        }
        if down && self.hk_switch.matches(mods_before, vk) {
            self.swallow[vki] = true;
            self.toggle();
            return true;
        }
        if down && self.hk_capture.matches(mods_before, vk) {
            if let Mode::Remote { pos, capture } = self.mode {
                self.swallow[vki] = true;
                let capture = !capture;
                self.mode = Mode::Remote { pos, capture };
                self.shared.capture.store(capture, Ordering::Relaxed);
                self.send(Event::SetMode { capture });
                info!(
                    "mouse mode: {}",
                    if capture { "capture (game)" } else { "monitor" }
                );
                return true;
            }
        }

        if !self.is_remote() {
            self.local_keys[vki] = down;
            return false;
        }
        if !down && self.local_keys[vki] {
            self.local_keys[vki] = false;
            return false;
        }
        let Some(code) = win_to_evdev(scan, extended, vk) else {
            debug!("unmapped key vk={vk:#x} scan={scan:#x} ext={extended}");
            return true;
        };
        let held = self.remote_keys.contains(&code);
        if down && !held {
            self.set_remote_key(code, true);
            self.send(Event::Key { code, down: true });
        } else if !down && held {
            self.set_remote_key(code, false);
            self.send(Event::Key { code, down: false });
        }
        // Windows auto-repeat (down while held) is dropped: the client's
        // compositor does its own key repeat.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Rect, Side};
    use std::sync::mpsc::{channel, Receiver};

    fn setup() -> (Engine, Receiver<Event>, Arc<Shared>) {
        let shared = Shared::new();
        shared.alive.store(true, Ordering::Relaxed);
        shared
            .client_size
            .store(1280 << 16 | 800, Ordering::Relaxed);
        let (tx, rx) = channel();
        let layout = Layout {
            monitors: vec![Rect {
                l: 0,
                t: 0,
                r: 1920,
                b: 1080,
            }],
            side: Side::Right,
            offset: 0,
            scale: 1.0,
            client_w: 0,
            client_h: 0,
        };
        let e = Engine::new(
            layout,
            tx,
            shared.clone(),
            Hotkey::parse("ctrl+alt+shift+s").unwrap(),
            Hotkey::parse("ctrl+alt+shift+m").unwrap(),
        );
        (e, rx, shared)
    }

    fn drain(rx: &Receiver<Event>) -> Vec<Event> {
        rx.try_iter().collect()
    }

    fn key(e: &mut Engine, vk: u32, scan: u32, down: bool) -> bool {
        e.input(Input::Key {
            vk,
            scan,
            extended: false,
            down,
        })
    }

    #[test]
    fn edge_cross_type_and_return() {
        let (mut e, rx, shared) = setup();
        assert!(!e.input(Input::Move {
            old: (1900, 500),
            new: (1910, 500),
            injected: false
        }));
        assert!(e.input(Input::Move {
            old: (1910, 500),
            new: (1925, 500),
            injected: false
        }));
        assert!(e.is_remote() && shared.active.load(Ordering::Relaxed));
        assert_eq!(
            drain(&rx),
            vec![Event::Enter {
                x: 6,
                y: 500,
                capture: false
            }]
        );
        assert_eq!(
            std::mem::take(&mut e.actions),
            vec![Action::HideCursor, Action::Warp(960, 540)]
        );

        // Cursor parked at 1919; hook reports parked + delta.
        assert!(e.input(Input::Move {
            old: (1919, 500),
            new: (1929, 490),
            injected: false
        }));
        assert!(key(&mut e, 0x41, 0x1E, true));
        assert!(key(&mut e, 0x41, 0x1E, true)); // auto-repeat dropped
        assert!(key(&mut e, 0x41, 0x1E, false));
        assert!(e.input(Input::Wheel {
            horizontal: false,
            delta: -120
        }));
        assert_eq!(
            drain(&rx),
            vec![
                Event::MoveAbs { x: 16, y: 490 },
                Event::Key {
                    code: 30,
                    down: true
                },
                Event::Key {
                    code: 30,
                    down: false
                },
                Event::Wheel {
                    horizontal: false,
                    v120: -120
                },
            ]
        );
        // move back left past the edge
        assert!(e.input(Input::Move {
            old: (1919, 490),
            new: (1899, 490),
            injected: false
        }));
        assert!(!e.is_remote());
        assert_eq!(drain(&rx), vec![Event::Leave]);
        assert_eq!(
            std::mem::take(&mut e.actions),
            vec![Action::Warp(1915, 490), Action::ShowCursor]
        );
    }

    #[test]
    fn no_switch_while_dragging_or_offline() {
        let (mut e, rx, shared) = setup();
        e.input(Input::Button { idx: 0, down: true });
        assert!(!e.input(Input::Move {
            old: (1918, 500),
            new: (1925, 500),
            injected: false
        }));
        e.input(Input::Button {
            idx: 0,
            down: false,
        });
        shared.alive.store(false, Ordering::Relaxed);
        assert!(!e.input(Input::Move {
            old: (1918, 500),
            new: (1925, 500),
            injected: false
        }));
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn hotkeys_and_held_keys() {
        let (mut e, rx, shared) = setup();
        // ctrl+alt+shift held locally
        for vk in [0xA2, 0xA4, 0xA0] {
            assert!(!key(&mut e, vk, 0, true));
        }
        assert!(key(&mut e, 0x53, 0x1F, true)); // S -> switch to client center
        assert_eq!(
            drain(&rx),
            vec![Event::Enter {
                x: 640,
                y: 400,
                capture: false
            }]
        );
        assert!(key(&mut e, 0x53, 0x1F, false)); // swallowed
                                                 // modifier releases go to Windows (they were pressed locally)
        for vk in [0xA2, 0xA4, 0xA0] {
            assert!(!key(&mut e, vk, 0, false));
        }
        assert!(drain(&rx).is_empty());

        // capture toggle while remote: Ctrl+Alt+Shift+M
        for (vk, sc) in [(0xA2, 0x1D), (0xA4, 0x38), (0xA0, 0x2A)] {
            assert!(key(&mut e, vk, sc, true)); // sent to client
        }
        assert!(key(&mut e, 0x4D, 0x32, true));
        assert!(shared.capture.load(Ordering::Relaxed));
        let evs = drain(&rx);
        assert_eq!(evs.last(), Some(&Event::SetMode { capture: true }));
        assert!(key(&mut e, 0x4D, 0x32, false));
        // in capture mode, motion is relative; with no raw input, hook deltas are used
        assert!(e.input(Input::Move {
            old: (100, 100),
            new: (103, 98),
            injected: false
        }));
        assert_eq!(drain(&rx), vec![Event::Motion { dx: 3, dy: -2 }]);
        // once raw input flows, it takes over
        e.input(Input::Raw { dx: 7, dy: 1 });
        assert!(e.input(Input::Move {
            old: (100, 100),
            new: (110, 100),
            injected: false
        }));
        assert_eq!(drain(&rx), vec![Event::Motion { dx: 7, dy: 1 }]);
        // switch hotkey leaves; client side held modifiers get released by Leave
        assert!(key(&mut e, 0x53, 0x1F, true));
        assert!(!e.is_remote());
        assert_eq!(drain(&rx), vec![Event::Leave]);
        // Releasing modifiers that were held on the client passes through to
        // Windows as a harmless stray key-up.
        assert!(!key(&mut e, 0xA2, 0x1D, false));
    }

    #[test]
    fn heartbeat_bitmap_tracks_remote_keys() {
        let (mut e, _rx, shared) = setup();
        e.input(Input::Move {
            old: (1918, 10),
            new: (1919, 10),
            injected: false,
        });
        assert!(e.is_remote());
        key(&mut e, 0x41, 0x1E, true);
        e.input(Input::Button { idx: 1, down: true });
        {
            let p = shared.pressed.lock().unwrap();
            assert!(glidekm_core::event::bit_get(&p, 30));
            assert!(glidekm_core::event::bit_get(&p, 0x111));
        }
        shared.alive.store(false, Ordering::Relaxed);
        e.input(Input::Tick);
        assert!(!e.is_remote());
        assert!(!glidekm_core::event::bit_get(
            &shared.pressed.lock().unwrap(),
            30
        ));
    }
}
