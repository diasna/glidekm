//! Detects the logical screen area that the absolute pointer maps onto.
//!
//! - Game Mode (gamescope): gamescope drives one output (the HDMI/DP monitor
//!   when docked, else the built-in panel). We ask the kernel (DRM) which
//!   outputs are lit and at which *active* mode - not the monitor's
//!   preferred mode, which can differ (e.g. a 4K TV that Steam runs at 1080p).
//! - Desktop Mode (KDE): the pointer spans the whole desktop, so with an
//!   extended desktop (Deck screen + HDMI) we need the full arrangement. KDE's
//!   `kscreen-doctor` reports it (positions, rotation and scaling applied).
//! - Fallback: sysfs connector state (no permissions needed).

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub internal: bool,
    /// Active mode as scanned out (pixels, not rotated).
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub w: u16,
    pub h: u16,
    /// Human-readable origin, shown in glidekm Settings.
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    Gamescope,
    Kde,
    Unknown,
}

/// Which compositor is running (scans /proc; cheap).
fn session() -> Session {
    let Ok(rd) = fs::read_dir("/proc") else {
        return Session::Unknown;
    };
    let mut kde = false;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let comm = fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        match comm.trim() {
            "gamescope" | "gamescope-wl" => return Session::Gamescope,
            "kwin_wayland" | "kwin_x11" | "kwin" => kde = true,
            _ => {}
        }
    }
    if kde {
        Session::Kde
    } else {
        Session::Unknown
    }
}

/// Logical size of a single output as gamescope presents it: the Deck's
/// panel is natively portrait (800x1280) but shown landscape.
fn logical(o: &Output) -> (u16, u16) {
    if o.internal && o.h > o.w {
        (o.h, o.w)
    } else {
        (o.w, o.h)
    }
}

/// The output gamescope uses: an external display wins over the panel.
fn pick(outputs: &[Output]) -> Option<&Output> {
    outputs.iter().find(|o| !o.internal).or(outputs.first())
}

fn describe_single(o: &Output, how: &str) -> Detected {
    let (w, h) = logical(o);
    Detected {
        w,
        h,
        source: format!("{} ({how})", pretty(&o.name, o.internal)),
    }
}

fn pretty(name: &str, internal: bool) -> String {
    if internal {
        "built-in screen".into()
    } else {
        format!("external display {name}")
    }
}

/// Re-detect at most this often unless the DRM state changed.
pub struct Detector {
    last_full: Option<Instant>,
    last_sig: Vec<Output>,
    last: Option<Detected>,
}

impl Detector {
    pub fn new() -> Self {
        Detector {
            last_full: None,
            last_sig: Vec::new(),
            last: None,
        }
    }

    /// Current screen. Cheap to call every few seconds: the expensive KDE
    /// query only runs when outputs change or every 30 s.
    pub fn detect(&mut self) -> Option<Detected> {
        let drm = drm::active_outputs().unwrap_or_default();
        let stale = self
            .last_full
            .is_none_or(|t| t.elapsed() > Duration::from_secs(30));
        if drm != self.last_sig || stale || self.last.is_none() {
            self.last_sig = drm.clone();
            self.last_full = Some(Instant::now());
            self.last = detect_with(session(), &drm);
        }
        self.last.clone()
    }
}

fn detect_with(session: Session, drm: &[Output]) -> Option<Detected> {
    if session == Session::Kde {
        if let Some(d) = kde::desktop() {
            return Some(d);
        }
    }
    if let Some(o) = pick(drm) {
        let how = if drm.len() > 1 {
            "active mode; others ignored"
        } else {
            "active mode"
        };
        return Some(describe_single(o, how));
    }
    sysfs::detect()
}

// ---------------------------------------------------------------- DRM

mod drm {
    //! Read-only KMS queries (no DRM master needed; same as `modetest`).
    use super::Output;
    use std::os::fd::AsRawFd;

    const fn iowr(nr: u32, size: usize) -> u32 {
        (3 << 30) | ((size as u32) << 16) | ((b'd' as u32) << 8) | nr
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct CardRes {
        fb_id_ptr: u64,
        crtc_id_ptr: u64,
        connector_id_ptr: u64,
        encoder_id_ptr: u64,
        count_fbs: u32,
        count_crtcs: u32,
        count_connectors: u32,
        count_encoders: u32,
        min_width: u32,
        max_width: u32,
        min_height: u32,
        max_height: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ModeInfo {
        clock: u32,
        hdisplay: u16,
        hsync_start: u16,
        hsync_end: u16,
        htotal: u16,
        hskew: u16,
        vdisplay: u16,
        vsync_start: u16,
        vsync_end: u16,
        vtotal: u16,
        vscan: u16,
        vrefresh: u32,
        flags: u32,
        type_: u32,
        name: [u8; 32],
    }

    #[repr(C)]
    pub struct Crtc {
        set_connectors_ptr: u64,
        count_connectors: u32,
        crtc_id: u32,
        fb_id: u32,
        x: u32,
        y: u32,
        gamma_size: u32,
        mode_valid: u32,
        mode: ModeInfo,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Encoder {
        encoder_id: u32,
        encoder_type: u32,
        crtc_id: u32,
        possible_crtcs: u32,
        possible_clones: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Connector {
        encoders_ptr: u64,
        modes_ptr: u64,
        props_ptr: u64,
        prop_values_ptr: u64,
        count_modes: u32,
        count_props: u32,
        count_encoders: u32,
        encoder_id: u32,
        connector_id: u32,
        connector_type: u32,
        connector_type_id: u32,
        connection: u32,
        mm_width: u32,
        mm_height: u32,
        subpixel: u32,
        pad: u32,
    }

    pub const GETRESOURCES: u32 = iowr(0xA0, std::mem::size_of::<CardRes>());
    pub const GETCRTC: u32 = iowr(0xA1, std::mem::size_of::<Crtc>());
    pub const GETENCODER: u32 = iowr(0xA6, std::mem::size_of::<Encoder>());
    pub const GETCONNECTOR: u32 = iowr(0xA7, std::mem::size_of::<Connector>());

    fn ioctl<T>(fd: i32, req: u32, arg: &mut T) -> bool {
        unsafe { libc::ioctl(fd, req as _, arg as *mut T) == 0 }
    }

    pub fn type_name(t: u32) -> (&'static str, bool) {
        match t {
            7 => ("LVDS", true),
            14 => ("eDP", true),
            16 => ("DSI", true),
            10 => ("DP", false),
            11 => ("HDMI-A", false),
            12 => ("HDMI-B", false),
            1 => ("VGA", false),
            2..=4 => ("DVI", false),
            15 => ("Virtual", false),
            20 => ("USB", false),
            _ => ("Unknown", false),
        }
    }

    /// Outputs currently lit, with their active modes, across all cards.
    pub fn active_outputs() -> Option<Vec<Output>> {
        let mut out = Vec::new();
        let mut cards: Vec<_> = std::fs::read_dir("/dev/dri")
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("card"))
            })
            .collect();
        cards.sort();
        for path in cards {
            let Ok(f) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            else {
                continue;
            };
            out.extend(card_outputs(f.as_raw_fd()));
        }
        Some(out)
    }

    fn card_outputs(fd: i32) -> Vec<Output> {
        let mut res = CardRes::default();
        if !ioctl(fd, GETRESOURCES, &mut res) || res.count_connectors == 0 {
            return Vec::new();
        }
        // Second call fills the id arrays (counts may grow in between; cap).
        let n = res.count_connectors.min(64) as usize;
        let mut conn_ids = vec![0u32; n];
        let mut res2 = CardRes {
            connector_id_ptr: conn_ids.as_mut_ptr() as u64,
            count_connectors: n as u32,
            ..Default::default()
        };
        if !ioctl(fd, GETRESOURCES, &mut res2) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for &cid in conn_ids
            .iter()
            .take(res2.count_connectors.min(n as u32) as usize)
        {
            // count_modes = 0 and not DRM master: the kernel does not re-probe.
            let mut c = Connector {
                connector_id: cid,
                ..Default::default()
            };
            if !ioctl(fd, GETCONNECTOR, &mut c) || c.connection != 1 || c.encoder_id == 0 {
                continue;
            }
            let mut e = Encoder {
                encoder_id: c.encoder_id,
                ..Default::default()
            };
            if !ioctl(fd, GETENCODER, &mut e) || e.crtc_id == 0 {
                continue;
            }
            let mut crtc: Crtc = unsafe { std::mem::zeroed() };
            crtc.crtc_id = e.crtc_id;
            if !ioctl(fd, GETCRTC, &mut crtc) || crtc.mode_valid == 0 {
                continue;
            }
            let (tname, internal) = type_name(c.connector_type);
            out.push(Output {
                name: format!("{tname}-{}", c.connector_type_id),
                internal,
                w: crtc.mode.hdisplay,
                h: crtc.mode.vdisplay,
            });
        }
        out
    }
}

// ---------------------------------------------------------------- KDE

mod kde {
    use super::*;

    /// Union of all enabled outputs in KDE's logical coordinates.
    pub fn desktop() -> Option<Detected> {
        let out = run_with_timeout("kscreen-doctor", &["-o"], Duration::from_secs(3))?;
        parse(&out)
    }

    fn run_with_timeout(cmd: &str, args: &[&str], t: Duration) -> Option<String> {
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let t0 = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(st)) if st.success() => break,
                Ok(Some(_)) => return None,
                Ok(None) if t0.elapsed() > t => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return None,
            }
        }
        let mut s = String::new();
        use std::io::Read;
        child.stdout?.read_to_string(&mut s).ok()?;
        Some(s)
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                // ESC [ ... letter
                if it.peek() == Some(&'[') {
                    it.next();
                    for d in it.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    /// Parse `kscreen-doctor -o` (Plasma 5 one-line or Plasma 6 multi-line
    /// format). Geometry is already logical (rotation and scale applied).
    pub fn parse(raw: &str) -> Option<Detected> {
        let text = strip_ansi(raw);
        let mut rects: Vec<(i32, i32, i32, i32, String)> = Vec::new();
        for block in text.split("Output:").skip(1) {
            let words: Vec<&str> = block.split_whitespace().collect();
            let name = words.get(1).copied().unwrap_or("?").to_string();
            if !words.contains(&"enabled") {
                continue;
            }
            let Some(i) = words.iter().position(|w| *w == "Geometry:") else {
                continue;
            };
            let (Some(pos), Some(size)) = (words.get(i + 1), words.get(i + 2)) else {
                continue;
            };
            let Some((x, y)) = pos.split_once(',') else {
                continue;
            };
            let Some((w, h)) = size.split_once('x') else {
                continue;
            };
            let (Ok(x), Ok(y), Ok(w), Ok(h)) =
                (x.parse(), y.parse(), w.parse::<i32>(), h.parse::<i32>())
            else {
                continue;
            };
            if w > 0 && h > 0 {
                rects.push((x, y, w, h, name));
            }
        }
        if rects.is_empty() {
            return None;
        }
        let l = rects.iter().map(|r| r.0).min()?;
        let t = rects.iter().map(|r| r.1).min()?;
        let r = rects.iter().map(|r| r.0 + r.2).max()?;
        let b = rects.iter().map(|r| r.1 + r.3).max()?;
        let (w, h) = (
            (r - l).clamp(1, 65535) as u16,
            (b - t).clamp(1, 65535) as u16,
        );
        let source = if rects.len() == 1 {
            format!("KDE desktop: {}", rects[0].4)
        } else {
            let names: Vec<&str> = rects.iter().map(|r| r.4.as_str()).collect();
            format!("KDE desktop spanning {}", names.join(" + "))
        };
        Some(Detected { w, h, source })
    }
}

// ---------------------------------------------------------------- sysfs fallback

mod sysfs {
    use super::*;

    /// Connected + enabled connectors, preferred mode (may differ from the
    /// active mode; only used when DRM queries aren't permitted).
    pub fn detect() -> Option<Detected> {
        let mut outs = Vec::new();
        for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("card") || !name.contains('-') {
                continue;
            }
            let p = entry.path();
            let read = |f: &str| fs::read_to_string(p.join(f)).unwrap_or_default();
            if read("status").trim() != "connected" || read("enabled").trim() == "disabled" {
                continue;
            }
            let Some((w, h)) = read("modes").lines().next().and_then(parse_mode) else {
                continue;
            };
            let conn = name
                .split_once('-')
                .map(|x| x.1)
                .unwrap_or(&name)
                .to_string();
            let internal =
                conn.starts_with("eDP") || conn.starts_with("LVDS") || conn.starts_with("DSI");
            outs.push(Output {
                name: conn,
                internal,
                w,
                h,
            });
        }
        pick(&outs).map(|o| describe_single(o, "preferred mode"))
    }
}

pub fn parse_mode(s: &str) -> Option<(u16, u16)> {
    let s = s.trim();
    let s = s
        .split(|c: char| !(c.is_ascii_digit() || c == 'x'))
        .next()?;
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, internal: bool, w: u16, h: u16) -> Output {
        Output {
            name: name.into(),
            internal,
            w,
            h,
        }
    }

    #[test]
    fn modes() {
        assert_eq!(parse_mode("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_mode("1920x1080i"), Some((1920, 1080)));
        assert_eq!(parse_mode("junk"), None);
    }

    #[test]
    fn drm_abi() {
        assert_eq!(drm::GETRESOURCES, 0xC04064A0);
        assert_eq!(drm::GETCRTC, 0xC06864A1);
        assert_eq!(drm::GETENCODER, 0xC01464A6);
        assert_eq!(drm::GETCONNECTOR, 0xC05064A7);
    }

    #[test]
    fn game_mode_choice() {
        // Undocked: portrait panel shown landscape.
        let d = detect_with(Session::Gamescope, &[out("eDP-1", true, 800, 1280)]).unwrap();
        assert_eq!((d.w, d.h), (1280, 800));
        assert_eq!(d.source, "built-in screen (active mode)");
        // Docked to a TV that Steam drives at 1080p: the active mode, external wins.
        let d = detect_with(
            Session::Gamescope,
            &[
                out("eDP-1", true, 800, 1280),
                out("HDMI-A-1", false, 1920, 1080),
            ],
        )
        .unwrap();
        assert_eq!((d.w, d.h), (1920, 1080));
        assert!(d.source.starts_with("external display HDMI-A-1"));
    }

    #[test]
    fn kde_plasma5_extended() {
        let txt = "Output: 1 eDP-1 enabled connected priority 2 Panel Modes: 0:800x1280@60*! Geometry: 0,0 1280x800 Scale: 1 Rotation: 8 Overscan: 0\n\
                   Output: 2 HDMI-A-1 enabled connected priority 1 HDMI Modes: 0:3840x2160@60! 1:1920x1080@60* Geometry: 1280,0 1920x1080 Scale: 1 Rotation: 1\n";
        let d = kde::parse(txt).unwrap();
        assert_eq!((d.w, d.h), (3200, 1080));
        assert_eq!(d.source, "KDE desktop spanning eDP-1 + HDMI-A-1");
    }

    #[test]
    fn kde_plasma6_multiline_disabled_and_ansi() {
        let txt = "\x1b[01;32mOutput: \x1b[0;0m1 eDP-1\n\tdisabled\n\tconnected\n\tGeometry: 0,0 1280x800\n\
                   \x1b[01;32mOutput: \x1b[0;0m2 DP-1\n\tenabled\n\tconnected\n\tpriority 1\n\tModes: 1:2560x1440@60*\n\tGeometry: 0,0 1707x960\n\tScale: 1.5\n";
        let d = kde::parse(txt).unwrap();
        assert_eq!((d.w, d.h), (1707, 960));
        assert_eq!(d.source, "KDE desktop: DP-1");
        assert!(kde::parse("no outputs here").is_none());
    }
}
