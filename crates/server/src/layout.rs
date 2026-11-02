//! "Extra monitor" geometry. Windows monitors live in physical pixels; the
//! client screen is a virtual monitor glued to one side of the virtual desktop.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32, // exclusive
    pub b: i32, // exclusive
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn parse(s: &str) -> Option<Side> {
        Some(match s.to_ascii_lowercase().as_str() {
            "left" => Side::Left,
            "right" => Side::Right,
            "top" | "above" => Side::Top,
            "bottom" | "below" => Side::Bottom,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub monitors: Vec<Rect>,
    pub side: Side,
    /// Offset of the client screen along the attached edge, Windows px.
    pub offset: i32,
    /// Windows px per client px.
    pub scale: f64,
    pub client_w: u16,
    pub client_h: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Still on the client; new position in client px.
    Stay(f64, f64),
    /// Leave to Windows at this point.
    Exit(i32, i32),
}

impl Layout {
    pub fn desktop(&self) -> Rect {
        let mut it = self.monitors.iter();
        let Some(first) = it.next() else {
            return Rect {
                l: 0,
                t: 0,
                r: 1,
                b: 1,
            };
        };
        it.fold(*first, |a, m| Rect {
            l: a.l.min(m.l),
            t: a.t.min(m.t),
            r: a.r.max(m.r),
            b: a.b.max(m.b),
        })
    }

    /// Client rect in Windows coordinates (floating point because of scale).
    pub fn client_rect(&self) -> (f64, f64, f64, f64) {
        let d = self.desktop();
        let w = self.client_w as f64 * self.scale;
        let h = self.client_h as f64 * self.scale;
        let o = self.offset as f64;
        match self.side {
            Side::Right => (d.r as f64, d.t as f64 + o, w, h),
            Side::Left => (d.l as f64 - w, d.t as f64 + o, w, h),
            Side::Bottom => (d.l as f64 + o, d.b as f64, w, h),
            Side::Top => (d.l as f64 + o, d.t as f64 - h, w, h),
        }
    }

    /// Is Windows point `p` on the attached outer edge at a place where a
    /// Windows monitor touches it *and* the client screen overlaps it?
    fn on_shared_edge(&self, along: f64) -> bool {
        let d = self.desktop();
        let (cx, cy, cw, ch) = self.client_rect();
        let (lo, hi) = match self.side {
            Side::Left | Side::Right => (cy, cy + ch),
            Side::Top | Side::Bottom => (cx, cx + cw),
        };
        if along < lo || along >= hi {
            return false;
        }
        let a = along.floor() as i32;
        self.monitors.iter().any(|m| match self.side {
            Side::Right => m.r == d.r && a >= m.t && a < m.b,
            Side::Left => m.l == d.l && a >= m.t && a < m.b,
            Side::Bottom => m.b == d.b && a >= m.l && a < m.r,
            Side::Top => m.t == d.t && a >= m.l && a < m.r,
        })
    }

    /// Local mouse move from `old` to `new` (as reported by the hook). If the
    /// cursor is pushed through the shared edge, returns the entry point in
    /// client px.
    pub fn try_enter(&self, old: (i32, i32), new: (i32, i32)) -> Option<(f64, f64)> {
        if self.client_w == 0 || self.client_h == 0 {
            return None;
        }
        let d = self.desktop();
        let (cx, cy, _, _) = self.client_rect();
        let s = self.scale;
        // outward: how far past the last pixel row/column the move goes (>= 0)
        let (outward, moving_out, along) = match self.side {
            Side::Right => (new.0 - (d.r - 1), new.0 > old.0, new.1),
            Side::Left => (d.l - new.0, new.0 < old.0, new.1),
            Side::Bottom => (new.1 - (d.b - 1), new.1 > old.1, new.0),
            Side::Top => (d.t - new.1, new.1 < old.1, new.0),
        };
        if outward < 0 || !moving_out || !self.on_shared_edge(along as f64) {
            return None;
        }
        let o = outward as f64 / s;
        let (w, h) = (self.client_w as f64, self.client_h as f64);
        let p = match self.side {
            Side::Right => (o, (along as f64 - cy) / s),
            Side::Left => (w - 1.0 - o, (along as f64 - cy) / s),
            Side::Bottom => ((along as f64 - cx) / s, o),
            Side::Top => ((along as f64 - cx) / s, h - 1.0 - o),
        };
        Some(self.clamp(p))
    }

    pub fn clamp(&self, (x, y): (f64, f64)) -> (f64, f64) {
        let w = (self.client_w as f64 - 1.0).max(0.0);
        let h = (self.client_h as f64 - 1.0).max(0.0);
        (x.clamp(0.0, w), y.clamp(0.0, h))
    }

    /// Apply a Windows-px delta while on the client.
    pub fn step(&self, pos: (f64, f64), d: (f64, f64)) -> Step {
        let s = self.scale;
        let (x, y) = (pos.0 + d.0 / s, pos.1 + d.1 / s);
        let (w, h) = (self.client_w as f64, self.client_h as f64);
        let desk = self.desktop();
        let (cx, cy, _, _) = self.client_rect();
        // Leaving happens only through the edge that faces Windows.
        let exit = match self.side {
            Side::Right if x < 0.0 => Some(((desk.r - 1) as f64 + x * s, cy + y * s, cy + y * s)),
            Side::Left if x > w - 1.0 => {
                Some((desk.l as f64 + (x - (w - 1.0)) * s, cy + y * s, cy + y * s))
            }
            Side::Bottom if y < 0.0 => Some((cx + x * s, (desk.b - 1) as f64 + y * s, cx + x * s)),
            Side::Top if y > h - 1.0 => {
                Some((cx + x * s, desk.t as f64 + (y - (h - 1.0)) * s, cx + x * s))
            }
            _ => None,
        };
        if let Some((wx, wy, along)) = exit {
            if self.on_shared_edge(along) {
                let (wx, wy) = (wx.floor() as i32, wy.floor() as i32);
                // Make sure we land inside some monitor.
                let (wx, wy) = match self.side {
                    Side::Right => (wx.max(desk.r - 1 - 4000).min(desk.r - 2), wy),
                    Side::Left => (wx.min(desk.l + 4000).max(desk.l + 1), wy),
                    Side::Bottom => (wx, wy.min(desk.b - 2)),
                    Side::Top => (wx, wy.max(desk.t + 1)),
                };
                return Step::Exit(wx, wy);
            }
        }
        let (x, y) = self.clamp((x, y));
        Step::Stay(x, y)
    }

    /// Center of the monitor containing `near` (or the first monitor).
    pub fn park_point(&self, near: Option<(i32, i32)>) -> (i32, i32) {
        let m = near
            .and_then(|(x, y)| {
                self.monitors
                    .iter()
                    .find(|m| x >= m.l && x < m.r && y >= m.t && y < m.b)
            })
            .or(self.monitors.first())
            .copied()
            .unwrap_or(Rect {
                l: 0,
                t: 0,
                r: 2,
                b: 2,
            });
        ((m.l + m.r) / 2, (m.t + m.b) / 2)
    }

    /// Where the Windows cursor should reappear when leaving by hotkey.
    pub fn exit_point_for(&self, pos: (f64, f64)) -> (i32, i32) {
        let d = self.desktop();
        let (cx, cy, _, _) = self.client_rect();
        let s = self.scale;
        let (x, y) = match self.side {
            Side::Right => ((d.r - 2) as f64, cy + pos.1 * s),
            Side::Left => ((d.l + 1) as f64, cy + pos.1 * s),
            Side::Bottom => (cx + pos.0 * s, (d.b - 2) as f64),
            Side::Top => (cx + pos.0 * s, (d.t + 1) as f64),
        };
        (
            x.clamp(d.l as f64, (d.r - 1) as f64) as i32,
            y.clamp(d.t as f64, (d.b - 1) as f64) as i32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two monitors side by side, the right one shorter, client on the right
    // with a vertical offset of 280.
    fn layout() -> Layout {
        Layout {
            monitors: vec![
                Rect {
                    l: 0,
                    t: 0,
                    r: 2560,
                    b: 1440,
                },
                Rect {
                    l: 2560,
                    t: 0,
                    r: 4480,
                    b: 1080,
                },
            ],
            side: Side::Right,
            offset: 280,
            scale: 1.0,
            client_w: 1280,
            client_h: 800,
        }
    }

    #[test]
    fn enters_through_shared_edge_only() {
        let l = layout();
        // y=500 is shared (client 280..1080, monitor 0..1080)
        assert_eq!(l.try_enter((4478, 500), (4479, 500)), Some((0.0, 220.0)));
        // overshoot carried over
        assert_eq!(l.try_enter((4470, 500), (4485, 500)), Some((6.0, 220.0)));
        // above the client: wall
        assert_eq!(l.try_enter((4478, 100), (4479, 100)), None);
        // not moving outward
        assert_eq!(l.try_enter((4479, 500), (4479, 501)), None);
        // not at edge
        assert_eq!(l.try_enter((4000, 500), (4001, 500)), None);
    }

    #[test]
    fn moves_and_exits() {
        let l = layout();
        assert_eq!(l.step((10.0, 10.0), (5.0, -20.0)), Step::Stay(15.0, 0.0));
        assert_eq!(
            l.step((1279.0, 10.0), (50.0, 0.0)),
            Step::Stay(1279.0, 10.0)
        );
        // leave left at client y=220 -> windows y=500
        assert_eq!(l.step((2.0, 220.0), (-5.0, 0.0)), Step::Exit(4476, 500));
        // client y=790 -> windows y=1070, still on the 1080-high monitor: exit
        assert!(matches!(
            l.step((0.0, 790.0), (-1.0, 0.0)),
            Step::Exit(_, 1070)
        ));
        // client y=0 maps to windows y=280: shared
        assert!(matches!(
            l.step((0.0, 0.0), (-1.0, 0.0)),
            Step::Exit(_, 280)
        ));
    }

    #[test]
    fn wall_where_no_monitor_touches() {
        // client taller than the right monitor: lower part has no neighbour
        let mut l = layout();
        l.offset = 600; // client covers windows y 600..1400, monitor 2 ends at 1080
        assert_eq!(l.step((0.0, 600.0), (-3.0, 0.0)), Step::Stay(0.0, 600.0));
        assert!(matches!(
            l.step((0.0, 100.0), (-3.0, 0.0)),
            Step::Exit(_, 700)
        ));
    }

    #[test]
    fn scaled_and_other_sides() {
        let mut l = layout();
        l.scale = 2.0;
        assert_eq!(l.try_enter((4478, 480), (4483, 480)), Some((2.0, 100.0)));
        l.side = Side::Left;
        l.offset = 0;
        let e = l.try_enter((1, 300), (-3, 300)).unwrap();
        assert_eq!(e, (1279.0 - 1.5, 150.0));
        assert!(matches!(
            l.step((1279.0, 150.0), (4.0, 0.0)),
            Step::Exit(_, 300)
        ));
        l.side = Side::Bottom;
        l.scale = 1.0;
        assert_eq!(l.try_enter((100, 1438), (100, 1440)), Some((100.0, 1.0)));
        assert!(matches!(
            l.step((100.0, 0.0), (0.0, -2.0)),
            Step::Exit(100, 1437)
        ));
        l.side = Side::Top;
        assert_eq!(l.try_enter((100, 1), (100, 0)), Some((100.0, 799.0)));
    }
}
