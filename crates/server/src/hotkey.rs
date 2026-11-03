//! Hotkey parsing ("ctrl+alt+shift+s") into modifier mask + Windows virtual key.

pub const MOD_CTRL: u8 = 1;
pub const MOD_ALT: u8 = 2;
pub const MOD_SHIFT: u8 = 4;
pub const MOD_WIN: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u8,
    pub vk: u32,
}

impl Hotkey {
    pub fn parse(s: &str) -> Result<Hotkey, String> {
        let mut mods = 0;
        let mut vk = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => mods |= MOD_CTRL,
                "alt" => mods |= MOD_ALT,
                "shift" => mods |= MOD_SHIFT,
                "win" | "super" | "meta" => mods |= MOD_WIN,
                k => {
                    if vk.is_some() {
                        return Err(format!("hotkey '{s}': more than one non-modifier key"));
                    }
                    vk = Some(key_vk(k).ok_or_else(|| format!("hotkey '{s}': unknown key '{k}'"))?);
                }
            }
        }
        Ok(Hotkey {
            mods,
            vk: vk.ok_or_else(|| format!("hotkey '{s}': missing key"))?,
        })
    }

    pub fn matches(&self, mods: u8, vk: u32) -> bool {
        self.vk == vk && self.mods == mods
    }

    /// Config-file form, e.g. "ctrl+alt+shift+s". None if the key has no name.
    pub fn to_config(self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        for (bit, name) in [
            (MOD_CTRL, "ctrl"),
            (MOD_ALT, "alt"),
            (MOD_SHIFT, "shift"),
            (MOD_WIN, "win"),
        ] {
            if self.mods & bit != 0 {
                parts.push(name.into());
            }
        }
        parts.push(vk_name(self.vk)?);
        Some(parts.join("+"))
    }
}

fn vk_name(vk: u32) -> Option<String> {
    Some(match vk {
        0x30..=0x39 => ((vk as u8) as char).to_string(),
        0x41..=0x5A => ((vk as u8).to_ascii_lowercase() as char).to_string(),
        0x70..=0x87 => format!("f{}", vk - 0x70 + 1),
        0x20 => "space".into(),
        0x09 => "tab".into(),
        0x1B => "esc".into(),
        0x0D => "enter".into(),
        0x08 => "backspace".into(),
        0x91 => "scrolllock".into(),
        0x13 => "pause".into(),
        0x2D => "insert".into(),
        0x2E => "delete".into(),
        0x24 => "home".into(),
        0x23 => "end".into(),
        0x21 => "pageup".into(),
        0x22 => "pagedown".into(),
        0x25 => "left".into(),
        0x26 => "up".into(),
        0x27 => "right".into(),
        0x28 => "down".into(),
        0xC0 => "`".into(),
        _ => return None,
    })
}

fn key_vk(k: &str) -> Option<u32> {
    let b = k.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_alphanumeric()) {
        return Some(b[0].to_ascii_uppercase() as u32);
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    Some(match k {
        "space" => 0x20,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "enter" | "return" => 0x0D,
        "backspace" => 0x08,
        "scrolllock" | "scroll" => 0x91,
        "pause" => 0x13,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "`" | "backtick" | "grave" => 0xC0,
        _ => return None,
    })
}

/// Map a (left/right specific) virtual key to its modifier bit.
pub fn vk_modifier(vk: u32) -> u8 {
    match vk {
        0xA2 | 0xA3 | 0x11 => MOD_CTRL,
        0xA4 | 0xA5 | 0x12 => MOD_ALT,
        0xA0 | 0xA1 | 0x10 => MOD_SHIFT,
        0x5B | 0x5C => MOD_WIN,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse() {
        assert_eq!(
            Hotkey::parse("Ctrl+Alt+Shift+S").unwrap(),
            Hotkey { mods: 7, vk: 0x53 }
        );
        assert_eq!(
            Hotkey::parse("win+f12").unwrap(),
            Hotkey {
                mods: MOD_WIN,
                vk: 0x7B
            }
        );
        assert_eq!(
            Hotkey::parse("scrolllock").unwrap(),
            Hotkey { mods: 0, vk: 0x91 }
        );
        assert!(Hotkey::parse("ctrl+").is_err());
        assert!(Hotkey::parse("ctrl+a+b").is_err());
        assert_eq!(vk_modifier(0xA3), MOD_CTRL);
        for t in [
            "ctrl+alt+shift+s",
            "win+f12",
            "scrolllock",
            "ctrl+`",
            "alt+pagedown",
            "shift+7",
        ] {
            assert_eq!(Hotkey::parse(t).unwrap().to_config().unwrap(), t);
        }
    }
}
