//! Client state machine: applies server events to the sink and guarantees
//! nothing stays pressed when the link or the server goes away.

use crate::sink::Sink;
use crate::uinput::ABS_MAX;
use glidekm_core::event::{bit_get, Event, KEY_BITS};
use glidekm_core::keymap::MOUSE_BUTTONS;

pub struct State<S: Sink> {
    pub sink: S,
    pub active: bool,
    pub capture: bool,
    pub width: u16,
    pub height: u16,
    keys: Vec<u16>,
    buttons: Vec<u16>,
}

impl<S: Sink> State<S> {
    pub fn new(sink: S, width: u16, height: u16) -> Self {
        State {
            sink,
            active: false,
            capture: false,
            width,
            height,
            keys: Vec::new(),
            buttons: Vec::new(),
        }
    }

    fn to_abs(v: u16, size: u16) -> i32 {
        let max = size.saturating_sub(1).max(1) as i64;
        ((v.min(size.saturating_sub(1)) as i64 * ABS_MAX as i64 + max / 2) / max) as i32
    }

    fn release_buttons(&mut self) {
        for b in std::mem::take(&mut self.buttons) {
            self.sink.button(self.capture, b, false);
        }
    }

    pub fn release_all(&mut self) {
        for k in std::mem::take(&mut self.keys) {
            self.sink.key(k, false);
        }
        self.release_buttons();
        self.sink.flush();
    }

    fn set_capture(&mut self, capture: bool) {
        if capture != self.capture {
            self.release_buttons();
            self.capture = capture;
        }
    }

    pub fn deactivate(&mut self) {
        self.release_all();
        self.active = false;
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Enter { x, y, capture } => {
                self.release_all();
                self.active = true;
                self.capture = capture;
                if !capture {
                    self.move_abs(x, y);
                }
            }
            Event::Leave => self.deactivate(),
            Event::MoveAbs { x, y } if self.active && !self.capture => self.move_abs(x, y),
            Event::Motion { dx, dy } if self.active && self.capture => {
                self.sink.move_rel(dx as i32, dy as i32)
            }
            Event::Button { code, down } if self.active && MOUSE_BUTTONS.contains(&code) => {
                let held = self.buttons.contains(&code);
                if down && !held {
                    self.buttons.push(code);
                    self.sink.button(self.capture, code, true);
                } else if !down && held {
                    self.buttons.retain(|&b| b != code);
                    self.sink.button(self.capture, code, false);
                }
            }
            Event::Wheel { horizontal, v120 } if self.active => {
                self.sink.wheel(self.capture, horizontal, v120 as i32)
            }
            Event::Key { code, down }
                if self.active && (code as usize) < KEY_BITS && code > 0 && code <= 248 =>
            {
                let held = self.keys.contains(&code);
                if down && !held {
                    self.keys.push(code);
                    self.sink.key(code, true);
                } else if !down && held {
                    self.keys.retain(|&k| k != code);
                    self.sink.key(code, false);
                }
            }
            Event::SetMode { capture } if self.active => self.set_capture(capture),
            Event::Heartbeat {
                active,
                capture,
                pressed,
            } => {
                if !active {
                    if self.active {
                        self.deactivate();
                    }
                } else {
                    self.active = true;
                    self.set_capture(capture);
                    // Release anything the server no longer considers held
                    // (covers lost key-up packets).
                    let stale_k: Vec<u16> = self
                        .keys
                        .iter()
                        .copied()
                        .filter(|&k| !bit_get(&pressed, k))
                        .collect();
                    for k in stale_k {
                        self.keys.retain(|&x| x != k);
                        self.sink.key(k, false);
                    }
                    let stale_b: Vec<u16> = self
                        .buttons
                        .iter()
                        .copied()
                        .filter(|&b| !bit_get(&pressed, b))
                        .collect();
                    for b in stale_b {
                        self.buttons.retain(|&x| x != b);
                        self.sink.button(self.capture, b, false);
                    }
                }
            }
            _ => {}
        }
    }

    fn move_abs(&mut self, x: u16, y: u16) {
        let (ax, ay) = (Self::to_abs(x, self.width), Self::to_abs(y, self.height));
        self.sink.move_abs(ax, ay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::Recorder;
    use glidekm_core::event::{bit_set, KEY_BITMAP_LEN};

    fn st() -> State<Recorder> {
        State::new(Recorder::default(), 1280, 800)
    }

    #[test]
    fn ignores_input_while_inactive() {
        let mut s = st();
        s.handle(Event::Key {
            code: 30,
            down: true,
        });
        s.handle(Event::MoveAbs { x: 1, y: 1 });
        assert!(s.sink.log.is_empty());
    }

    #[test]
    fn enter_move_type_leave() {
        let mut s = st();
        s.handle(Event::Enter {
            x: 0,
            y: 799,
            capture: false,
        });
        s.handle(Event::MoveAbs { x: 1279, y: 0 });
        s.handle(Event::Key {
            code: 30,
            down: true,
        });
        s.handle(Event::Key {
            code: 30,
            down: true,
        }); // repeat ignored
        s.handle(Event::Button {
            code: 0x110,
            down: true,
        });
        s.handle(Event::Leave);
        assert_eq!(
            s.sink.log,
            vec![
                "abs 0 32767",
                "abs 32767 0",
                "key 30 down",
                "button 0x110 down",
                "key 30 up",
                "button 0x110 up"
            ]
        );
        assert!(!s.active);
    }

    #[test]
    fn capture_mode_uses_relative_device() {
        let mut s = st();
        s.handle(Event::Enter {
            x: 10,
            y: 10,
            capture: false,
        });
        s.handle(Event::Button {
            code: 0x110,
            down: true,
        });
        s.handle(Event::SetMode { capture: true });
        s.handle(Event::Motion { dx: 5, dy: -3 });
        s.handle(Event::MoveAbs { x: 1, y: 1 }); // ignored in capture
        s.handle(Event::Button {
            code: 0x111,
            down: true,
        });
        assert_eq!(
            &s.sink.log[1..],
            &[
                "button 0x110 down",
                "button 0x110 up",
                "rel 5 -3",
                "button(rel) 0x111 down"
            ]
        );
    }

    #[test]
    fn heartbeat_releases_stuck_keys() {
        let mut s = st();
        s.handle(Event::Enter {
            x: 0,
            y: 0,
            capture: false,
        });
        s.handle(Event::Key {
            code: 30,
            down: true,
        });
        s.handle(Event::Key {
            code: 31,
            down: true,
        });
        let mut p = Box::new([0u8; KEY_BITMAP_LEN]);
        bit_set(&mut p, 31, true);
        s.handle(Event::Heartbeat {
            active: true,
            capture: false,
            pressed: p,
        });
        assert_eq!(s.sink.log.last().unwrap(), "key 30 up");
        s.handle(Event::Heartbeat {
            active: false,
            capture: false,
            pressed: Box::new([0; KEY_BITMAP_LEN]),
        });
        assert_eq!(s.sink.log.last().unwrap(), "key 31 up");
        assert!(!s.active);
    }
}
