//! Output side: where decoded input goes. Real uinput devices or a dry-run logger.

use crate::uinput::{self, Device, Spec};
use glidekm_core::keymap::MOUSE_BUTTONS;

pub trait Sink {
    fn key(&mut self, code: u16, down: bool);
    /// `capture` selects the relative (game) pointer, else the absolute pointer.
    fn button(&mut self, capture: bool, code: u16, down: bool);
    fn wheel(&mut self, capture: bool, horizontal: bool, v120: i32);
    /// Normalised absolute position, 0..=uinput::ABS_MAX.
    fn move_abs(&mut self, x: i32, y: i32);
    fn move_rel(&mut self, dx: i32, dy: i32);
    fn flush(&mut self);
}

pub struct Uinput {
    kbd: Device,
    abs: Device,
    rel: Device,
    // Remainders for low-resolution wheel events, per device and axis.
    wheel_acc: [[i32; 2]; 2],
}

const WHEELS: [u16; 4] = [
    uinput::REL_WHEEL,
    uinput::REL_HWHEEL,
    uinput::REL_WHEEL_HI_RES,
    uinput::REL_HWHEEL_HI_RES,
];

impl Uinput {
    pub fn new() -> std::io::Result<Uinput> {
        // KEY_ESC(1) ..= KEY_MICMUTE(248): everything a keyboard can produce.
        let keys: Vec<u16> = (1..=248).collect();
        let kbd = Device::create(&Spec {
            name: "glidekm keyboard",
            product: 0x0001,
            keys: &keys,
            rels: &[],
            abs: false,
            pointer: false,
        })?;
        let abs = Device::create(&Spec {
            name: "glidekm pointer",
            product: 0x0002,
            keys: &MOUSE_BUTTONS,
            rels: &WHEELS,
            abs: true,
            pointer: true,
        })?;
        let mut rels = vec![uinput::REL_X, uinput::REL_Y];
        rels.extend_from_slice(&WHEELS);
        let rel = Device::create(&Spec {
            name: "glidekm mouse",
            product: 0x0003,
            keys: &MOUSE_BUTTONS,
            rels: &rels,
            abs: false,
            pointer: true,
        })?;
        Ok(Uinput {
            kbd,
            abs,
            rel,
            wheel_acc: [[0; 2]; 2],
        })
    }

    fn ptr(&mut self, capture: bool) -> &mut Device {
        if capture {
            &mut self.rel
        } else {
            &mut self.abs
        }
    }
}

impl Sink for Uinput {
    fn key(&mut self, code: u16, down: bool) {
        self.kbd.push(uinput::EV_KEY, code, down as i32);
    }
    fn button(&mut self, capture: bool, code: u16, down: bool) {
        self.ptr(capture).push(uinput::EV_KEY, code, down as i32);
    }
    fn wheel(&mut self, capture: bool, horizontal: bool, v120: i32) {
        let acc = &mut self.wheel_acc[capture as usize][horizontal as usize];
        *acc += v120;
        let notches = *acc / 120;
        *acc -= notches * 120;
        let (lo, hi) = if horizontal {
            (uinput::REL_HWHEEL, uinput::REL_HWHEEL_HI_RES)
        } else {
            (uinput::REL_WHEEL, uinput::REL_WHEEL_HI_RES)
        };
        let d = self.ptr(capture);
        d.push(uinput::EV_REL, hi, v120);
        if notches != 0 {
            d.push(uinput::EV_REL, lo, notches);
        }
    }
    fn move_abs(&mut self, x: i32, y: i32) {
        self.abs.push(uinput::EV_ABS, uinput::ABS_X, x);
        self.abs.push(uinput::EV_ABS, uinput::ABS_Y, y);
    }
    fn move_rel(&mut self, dx: i32, dy: i32) {
        if dx != 0 {
            self.rel.push(uinput::EV_REL, uinput::REL_X, dx);
        }
        if dy != 0 {
            self.rel.push(uinput::EV_REL, uinput::REL_Y, dy);
        }
    }
    fn flush(&mut self) {
        self.kbd.flush();
        self.abs.flush();
        self.rel.flush();
    }
}

/// Logs what would be injected. Used with --dry-run and in tests.
#[derive(Default)]
pub struct Recorder {
    pub log: Vec<String>,
    pub echo: bool,
}

impl Recorder {
    fn rec(&mut self, s: String) {
        if self.echo {
            glidekm_core::info!("[dry-run] {s}");
        }
        self.log.push(s);
    }
}

impl Sink for Recorder {
    fn key(&mut self, code: u16, down: bool) {
        self.rec(format!("key {code} {}", if down { "down" } else { "up" }));
    }
    fn button(&mut self, capture: bool, code: u16, down: bool) {
        self.rec(format!(
            "button{} {code:#x} {}",
            if capture { "(rel)" } else { "" },
            if down { "down" } else { "up" }
        ));
    }
    fn wheel(&mut self, capture: bool, horizontal: bool, v120: i32) {
        self.rec(format!(
            "wheel{} {} {v120}",
            if capture { "(rel)" } else { "" },
            if horizontal { "h" } else { "v" }
        ));
    }
    fn move_abs(&mut self, x: i32, y: i32) {
        self.rec(format!("abs {x} {y}"));
    }
    fn move_rel(&mut self, dx: i32, dy: i32) {
        self.rec(format!("rel {dx} {dy}"));
    }
    fn flush(&mut self) {}
}

/// Real devices that keep retrying until /dev/uinput becomes accessible
/// (e.g. after the udev rule is installed), instead of exiting.
pub struct RetryUinput {
    dev: Option<Uinput>,
    last_try: Option<std::time::Instant>,
    pub error: String,
}

impl RetryUinput {
    pub fn new() -> Self {
        let mut s = RetryUinput {
            dev: None,
            last_try: None,
            error: String::new(),
        };
        s.tick();
        s
    }

    /// Try to (re)create the devices at most every 5 s while missing.
    pub fn tick(&mut self) {
        if self.dev.is_some()
            || self
                .last_try
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(5))
        {
            return;
        }
        self.last_try = Some(std::time::Instant::now());
        match Uinput::new() {
            Ok(d) => {
                glidekm_core::info!("uinput devices created");
                self.dev = Some(d);
                self.error.clear();
            }
            Err(e) => {
                let msg = format!("cannot open /dev/uinput: {e}");
                if msg != self.error {
                    glidekm_core::warn!(
                        "{msg} (retrying every 5 s; see settings page for the fix)"
                    );
                }
                self.error = msg;
            }
        }
    }
}

macro_rules! fwd {
    ($self:ident, $m:ident ( $($a:expr),* )) => {
        if let Some(d) = $self.dev.as_mut() { d.$m($($a),*) }
    };
}

impl Sink for RetryUinput {
    fn key(&mut self, code: u16, down: bool) {
        fwd!(self, key(code, down))
    }
    fn button(&mut self, capture: bool, code: u16, down: bool) {
        fwd!(self, button(capture, code, down))
    }
    fn wheel(&mut self, capture: bool, horizontal: bool, v120: i32) {
        fwd!(self, wheel(capture, horizontal, v120))
    }
    fn move_abs(&mut self, x: i32, y: i32) {
        fwd!(self, move_abs(x, y))
    }
    fn move_rel(&mut self, dx: i32, dy: i32) {
        fwd!(self, move_rel(dx, dy))
    }
    fn flush(&mut self) {
        fwd!(self, flush())
    }
}

/// The sink actually used by the client: real devices or a dry-run logger.
pub enum Output {
    Real(RetryUinput),
    Dry(Recorder),
}

impl Output {
    pub fn tick(&mut self) {
        if let Output::Real(r) = self {
            r.tick()
        }
    }
    pub fn error(&self) -> &str {
        match self {
            Output::Real(r) => &r.error,
            Output::Dry(_) => "",
        }
    }
    fn inner(&mut self) -> &mut dyn Sink {
        match self {
            Output::Real(r) => r,
            Output::Dry(d) => d,
        }
    }
}

impl Sink for Output {
    fn key(&mut self, code: u16, down: bool) {
        self.inner().key(code, down)
    }
    fn button(&mut self, capture: bool, code: u16, down: bool) {
        self.inner().button(capture, code, down)
    }
    fn wheel(&mut self, capture: bool, horizontal: bool, v120: i32) {
        self.inner().wheel(capture, horizontal, v120)
    }
    fn move_abs(&mut self, x: i32, y: i32) {
        self.inner().move_abs(x, y)
    }
    fn move_rel(&mut self, dx: i32, dy: i32) {
        self.inner().move_rel(dx, dy)
    }
    fn flush(&mut self) {
        self.inner().flush()
    }
}
