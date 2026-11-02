//! Best-effort detection of the logical screen size from DRM sysfs.
//! Works without a display server connection, so it behaves the same in
//! Game Mode (gamescope) and Desktop Mode.

use std::fs;

pub fn detect() -> Option<(u16, u16)> {
    let mut internal = None;
    let mut external = None;
    for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Connector directories look like card0-eDP-1, card1-DP-1, ...
        if !name.starts_with("card") || !name.contains('-') {
            continue;
        }
        let p = entry.path();
        if fs::read_to_string(p.join("status"))
            .map(|s| s.trim() != "connected")
            .unwrap_or(true)
        {
            continue;
        }
        let modes = fs::read_to_string(p.join("modes")).unwrap_or_default();
        let Some((w, h)) = modes.lines().next().and_then(parse_mode) else {
            continue;
        };
        if name.contains("eDP") || name.contains("LVDS") || name.contains("DSI") {
            internal = Some(logical_internal(w, h));
        } else if external.is_none() {
            external = Some((w, h));
        }
    }
    // When docked, gamescope renders to the external display.
    external.or(internal)
}

/// The Steam Deck panel is natively portrait (800x1280) but rotated to landscape.
fn logical_internal(w: u16, h: u16) -> (u16, u16) {
    if h > w {
        (h, w)
    } else {
        (w, h)
    }
}

fn parse_mode(s: &str) -> Option<(u16, u16)> {
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
    #[test]
    fn modes() {
        assert_eq!(parse_mode("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_mode("1920x1080i"), Some((1920, 1080)));
        assert_eq!(parse_mode("junk"), None);
        assert_eq!(logical_internal(800, 1280), (1280, 800));
    }
}
