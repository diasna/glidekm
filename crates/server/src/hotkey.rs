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
    }
}
