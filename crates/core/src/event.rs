//! Wire events. Key/button codes are Linux evdev codes. Several events may be
//! packed into one datagram; each starts with a 1-byte tag.

pub const KEY_BITS: usize = 768; // covers KEY_* and BTN_* used here (< 0x300)
pub const KEY_BITMAP_LEN: usize = KEY_BITS / 8;

pub type KeyBitmap = [u8; KEY_BITMAP_LEN];

pub fn bit_get(b: &KeyBitmap, code: u16) -> bool {
    let c = code as usize;
    c < KEY_BITS && b[c / 8] & (1 << (c % 8)) != 0
}

pub fn bit_set(b: &mut KeyBitmap, code: u16, on: bool) {
    let c = code as usize;
    if c < KEY_BITS {
        if on {
            b[c / 8] |= 1 << (c % 8)
        } else {
            b[c / 8] &= !(1 << (c % 8))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Server -> client: cursor entered the client screen at (x, y) client px.
    Enter {
        x: u16,
        y: u16,
        capture: bool,
    },
    /// Server -> client: control returned to the server.
    Leave,
    /// Absolute cursor position in client px (monitor mode).
    MoveAbs {
        x: u16,
        y: u16,
    },
    /// Relative motion (capture / game mode).
    Motion {
        dx: i16,
        dy: i16,
    },
    Button {
        code: u16,
        down: bool,
    },
    /// Wheel in 1/120 notch units (Windows WHEEL_DELTA == evdev hi-res units).
    Wheel {
        horizontal: bool,
        v120: i16,
    },
    Key {
        code: u16,
        down: bool,
    },
    SetMode {
        capture: bool,
    },
    /// Server -> client, periodically: authoritative state for reconciliation.
    Heartbeat {
        active: bool,
        capture: bool,
        pressed: Box<KeyBitmap>,
    },
    /// Client -> server: client is alive and its logical screen size.
    Hello {
        width: u16,
        height: u16,
    },
    /// Either direction: one chunk of a shared text clipboard.
    Clip {
        id: u32,
        /// 0 = UTF-8 text, 1 = PNG image.
        kind: u8,
        idx: u16,
        count: u16,
        data: Vec<u8>,
    },
    /// Either direction: "I'm missing these chunks of transfer `id`".
    ClipNack {
        id: u32,
        missing: Vec<u16>,
    },
}

mod tag {
    pub const ENTER: u8 = 1;
    pub const LEAVE: u8 = 2;
    pub const MOVE_ABS: u8 = 3;
    pub const MOTION: u8 = 4;
    pub const BUTTON: u8 = 5;
    pub const WHEEL: u8 = 6;
    pub const KEY: u8 = 7;
    pub const SET_MODE: u8 = 8;
    pub const HEARTBEAT: u8 = 9;
    pub const HELLO: u8 = 10;
    pub const CLIP: u8 = 11;
    pub const CLIP_NACK: u8 = 12;
}

impl Event {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Event::Enter { x, y, capture } => {
                out.push(tag::ENTER);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.push(*capture as u8);
            }
            Event::Leave => out.push(tag::LEAVE),
            Event::MoveAbs { x, y } => {
                out.push(tag::MOVE_ABS);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            Event::Motion { dx, dy } => {
                out.push(tag::MOTION);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            Event::Button { code, down } => {
                out.push(tag::BUTTON);
                out.extend_from_slice(&code.to_le_bytes());
                out.push(*down as u8);
            }
            Event::Wheel { horizontal, v120 } => {
                out.push(tag::WHEEL);
                out.push(*horizontal as u8);
                out.extend_from_slice(&v120.to_le_bytes());
            }
            Event::Key { code, down } => {
                out.push(tag::KEY);
                out.extend_from_slice(&code.to_le_bytes());
                out.push(*down as u8);
            }
            Event::SetMode { capture } => {
                out.push(tag::SET_MODE);
                out.push(*capture as u8);
            }
            Event::Heartbeat {
                active,
                capture,
                pressed,
            } => {
                out.push(tag::HEARTBEAT);
                out.push(*active as u8);
                out.push(*capture as u8);
                out.extend_from_slice(&pressed[..]);
            }
            Event::Hello { width, height } => {
                out.push(tag::HELLO);
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
            }
            Event::Clip {
                id,
                kind,
                idx,
                count,
                data,
            } => {
                out.push(tag::CLIP);
                out.extend_from_slice(&id.to_le_bytes());
                out.push(*kind);
                out.extend_from_slice(&idx.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
                out.extend_from_slice(&(data.len() as u16).to_le_bytes());
                out.extend_from_slice(data);
            }
            Event::ClipNack { id, missing } => {
                out.push(tag::CLIP_NACK);
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&(missing.len() as u16).to_le_bytes());
                for m in missing {
                    out.extend_from_slice(&m.to_le_bytes());
                }
            }
        }
    }

    /// Decode all events in a datagram. Stops (returning what it has) on garbage.
    pub fn decode_all(mut b: &[u8]) -> Vec<Event> {
        let mut v = Vec::new();
        while let Some((ev, n)) = Self::decode_one(b) {
            v.push(ev);
            b = &b[n..];
        }
        v
    }

    fn decode_one(b: &[u8]) -> Option<(Event, usize)> {
        let t = *b.first()?;
        let u16_at =
            |i: usize| -> Option<u16> { Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?])) };
        let i16_at = |i: usize| -> Option<i16> { u16_at(i).map(|v| v as i16) };
        let bool_at = |i: usize| -> Option<bool> { b.get(i).map(|v| *v != 0) };
        Some(match t {
            tag::ENTER => (
                Event::Enter {
                    x: u16_at(1)?,
                    y: u16_at(3)?,
                    capture: bool_at(5)?,
                },
                6,
            ),
            tag::LEAVE => (Event::Leave, 1),
            tag::MOVE_ABS => (
                Event::MoveAbs {
                    x: u16_at(1)?,
                    y: u16_at(3)?,
                },
                5,
            ),
            tag::MOTION => (
                Event::Motion {
                    dx: i16_at(1)?,
                    dy: i16_at(3)?,
                },
                5,
            ),
            tag::BUTTON => (
                Event::Button {
                    code: u16_at(1)?,
                    down: bool_at(3)?,
                },
                4,
            ),
            tag::WHEEL => (
                Event::Wheel {
                    horizontal: bool_at(1)?,
                    v120: i16_at(2)?,
                },
                4,
            ),
            tag::KEY => (
                Event::Key {
                    code: u16_at(1)?,
                    down: bool_at(3)?,
                },
                4,
            ),
            tag::SET_MODE => (
                Event::SetMode {
                    capture: bool_at(1)?,
                },
                2,
            ),
            tag::HEARTBEAT => {
                let bits = b.get(3..3 + KEY_BITMAP_LEN)?;
                let mut pressed = Box::new([0u8; KEY_BITMAP_LEN]);
                pressed.copy_from_slice(bits);
                (
                    Event::Heartbeat {
                        active: bool_at(1)?,
                        capture: bool_at(2)?,
                        pressed,
                    },
                    3 + KEY_BITMAP_LEN,
                )
            }
            tag::HELLO => (
                Event::Hello {
                    width: u16_at(1)?,
                    height: u16_at(3)?,
                },
                5,
            ),
            tag::CLIP => {
                let len = u16_at(10)? as usize;
                let data = b.get(12..12 + len)?.to_vec();
                let id = u32::from_le_bytes([*b.get(1)?, *b.get(2)?, *b.get(3)?, *b.get(4)?]);
                (
                    Event::Clip {
                        id,
                        kind: *b.get(5)?,
                        idx: u16_at(6)?,
                        count: u16_at(8)?,
                        data,
                    },
                    12 + len,
                )
            }
            tag::CLIP_NACK => {
                let n = u16_at(5)? as usize;
                let id = u32::from_le_bytes([*b.get(1)?, *b.get(2)?, *b.get(3)?, *b.get(4)?]);
                let missing = (0..n)
                    .map(|i| u16_at(7 + 2 * i))
                    .collect::<Option<Vec<_>>>()?;
                (Event::ClipNack { id, missing }, 7 + 2 * n)
            }
            _ => return None,
        })
    }
}

pub fn encode_all(evs: &[Event]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    for e in evs {
        e.encode(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut pressed = Box::new([0u8; KEY_BITMAP_LEN]);
        bit_set(&mut pressed, 30, true);
        bit_set(&mut pressed, 0x110, true);
        let evs = vec![
            Event::Enter {
                x: 1,
                y: 65535,
                capture: true,
            },
            Event::Leave,
            Event::MoveAbs { x: 640, y: 400 },
            Event::Motion {
                dx: -32768,
                dy: 32767,
            },
            Event::Button {
                code: 0x110,
                down: true,
            },
            Event::Wheel {
                horizontal: true,
                v120: -120,
            },
            Event::Key {
                code: 30,
                down: false,
            },
            Event::SetMode { capture: false },
            Event::Heartbeat {
                active: true,
                capture: false,
                pressed: pressed.clone(),
            },
            Event::Hello {
                width: 1280,
                height: 800,
            },
            Event::Clip {
                id: 0xdead_beef,
                kind: 1,
                idx: 1,
                count: 3,
                data: b"abc".to_vec(),
            },
            Event::ClipNack {
                id: 9,
                missing: vec![1, 500, 65535],
            },
        ];
        let bytes = encode_all(&evs);
        assert_eq!(Event::decode_all(&bytes), evs);
        assert!(bit_get(&pressed, 30) && bit_get(&pressed, 0x110) && !bit_get(&pressed, 31));
    }

    #[test]
    fn truncated_is_safe() {
        let bytes = encode_all(&[
            Event::Key {
                code: 30,
                down: true,
            },
            Event::Hello {
                width: 1,
                height: 2,
            },
        ]);
        for n in 0..bytes.len() {
            let _ = Event::decode_all(&bytes[..n]);
        }
        assert_eq!(
            Event::decode_all(&bytes[..5]),
            vec![Event::Key {
                code: 30,
                down: true
            }]
        );
        assert!(Event::decode_all(&[0xff, 1, 2]).is_empty());
    }
}
