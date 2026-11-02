//! Windows (scan code set 1 + E0 flag, virtual-key fallback) -> Linux evdev codes.

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const MOUSE_BUTTONS: [u16; 5] = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA];

// Windows virtual keys needing special handling.
const VK_PAUSE: u32 = 0x13;
const VK_NUMLOCK: u32 = 0x90;
const VK_SNAPSHOT: u32 = 0x2C;

/// Map a Windows low-level keyboard event to an evdev key code.
pub fn win_to_evdev(scan: u32, extended: bool, vk: u32) -> Option<u16> {
    // Ambiguous scan codes are resolved through the virtual key.
    match vk {
        VK_PAUSE => return Some(119),   // KEY_PAUSE
        VK_NUMLOCK => return Some(69),  // KEY_NUMLOCK
        VK_SNAPSHOT => return Some(99), // KEY_SYSRQ (PrintScreen)
        _ => {}
    }
    let code = if extended {
        extended_scan(scan)
    } else {
        plain_scan(scan)
    };
    code.or_else(|| vk_fallback(vk))
}

fn plain_scan(s: u32) -> Option<u16> {
    Some(match s {
        // Set-1 scan codes 0x01..=0x58 equal evdev codes (ESC=1 ... F12=88),
        // except 0x54 (Alt+SysRq) and the unused 0x55.
        0x54 => 99,
        0x55 => return None,
        0x01..=0x58 => s as u16,
        0x59 => 117, // KP_EQUAL
        0x5A..=0x63 => return None,
        0x64 => 183, // F13
        0x65 => 184,
        0x66 => 185,
        0x67 => 186,
        0x68 => 187,
        0x69 => 188,
        0x6A => 189,
        0x6B => 190,
        0x6C => 191,
        0x6D => 192,
        0x6E => 193, // F23
        0x70 => 93,  // KATAKANAHIRAGANA
        0x73 => 89,  // RO
        0x76 => 194, // F24
        0x79 => 92,  // HENKAN
        0x7B => 94,  // MUHENKAN
        0x7D => 124, // YEN
        0x7E => 121, // KPCOMMA
        0xF1 => 122, // HANGEUL
        0xF2 => 123, // HANJA
        _ => return None,
    })
}

fn extended_scan(s: u32) -> Option<u16> {
    Some(match s {
        0x10 => 165, // PREVIOUSSONG
        0x19 => 163, // NEXTSONG
        0x1C => 96,  // KPENTER
        0x1D => 97,  // RIGHTCTRL
        0x20 => 113, // MUTE
        0x21 => 140, // CALC
        0x22 => 164, // PLAYPAUSE
        0x24 => 166, // STOPCD
        0x2E => 114, // VOLUMEDOWN
        0x30 => 115, // VOLUMEUP
        0x32 => 172, // HOMEPAGE
        0x35 => 98,  // KPSLASH
        0x36 => 54,  // RIGHTSHIFT (some drivers flag it extended)
        0x37 => 99,  // SYSRQ
        0x38 => 100, // RIGHTALT
        0x45 => 69,  // NUMLOCK
        0x47 => 102, // HOME
        0x48 => 103, // UP
        0x49 => 104, // PAGEUP
        0x4B => 105, // LEFT
        0x4D => 106, // RIGHT
        0x4F => 107, // END
        0x50 => 108, // DOWN
        0x51 => 109, // PAGEDOWN
        0x52 => 110, // INSERT
        0x53 => 111, // DELETE
        0x5B => 125, // LEFTMETA
        0x5C => 126, // RIGHTMETA
        0x5D => 127, // COMPOSE (menu)
        0x5E => 116, // POWER
        0x5F => 142, // SLEEP
        0x63 => 143, // WAKEUP
        0x65 => 217, // SEARCH
        0x66 => 156, // BOOKMARKS
        0x67 => 173, // REFRESH
        0x68 => 128, // STOP
        0x69 => 159, // FORWARD
        0x6A => 158, // BACK
        0x6B => 157, // COMPUTER
        0x6C => 155, // MAIL
        0x6D => 226, // MEDIA
        _ => return None,
    })
}

/// Some keyboards (media keys, remapping tools) report scan code 0.
fn vk_fallback(vk: u32) -> Option<u16> {
    Some(match vk {
        0xAD => 113, // VOLUME_MUTE
        0xAE => 114, // VOLUME_DOWN
        0xAF => 115, // VOLUME_UP
        0xB0 => 163, // MEDIA_NEXT_TRACK
        0xB1 => 165, // MEDIA_PREV_TRACK
        0xB2 => 166, // MEDIA_STOP
        0xB3 => 164, // MEDIA_PLAY_PAUSE
        0x5B => 125, // LWIN
        0x5C => 126, // RWIN
        0x5D => 127, // APPS
        0xA6 => 158, // BROWSER_BACK
        0xA7 => 159, // BROWSER_FORWARD
        0xA8 => 173, // BROWSER_REFRESH
        0xAA => 217, // BROWSER_SEARCH
        0xAC => 172, // BROWSER_HOME
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(win_to_evdev(0x01, false, 0x1B), Some(1)); // ESC
        assert_eq!(win_to_evdev(0x1E, false, 0x41), Some(30)); // A
        assert_eq!(win_to_evdev(0x1C, false, 0x0D), Some(28)); // Enter
        assert_eq!(win_to_evdev(0x1C, true, 0x0D), Some(96)); // KP Enter
        assert_eq!(win_to_evdev(0x1D, false, 0xA2), Some(29)); // LCtrl
        assert_eq!(win_to_evdev(0x1D, true, 0xA3), Some(97)); // RCtrl
        assert_eq!(win_to_evdev(0x38, true, 0xA5), Some(100)); // RAlt
        assert_eq!(win_to_evdev(0x48, true, 0x26), Some(103)); // Up
        assert_eq!(win_to_evdev(0x48, false, 0x68), Some(72)); // KP8
        assert_eq!(win_to_evdev(0x57, false, 0x7A), Some(87)); // F11
        assert_eq!(win_to_evdev(0x58, false, 0x7B), Some(88)); // F12
        assert_eq!(win_to_evdev(0x5B, true, 0x5B), Some(125)); // LWin
        assert_eq!(win_to_evdev(0x45, false, 0x13), Some(119)); // Pause
        assert_eq!(win_to_evdev(0x45, true, 0x90), Some(69)); // NumLock
        assert_eq!(win_to_evdev(0x37, true, 0x2C), Some(99)); // PrtSc
        assert_eq!(win_to_evdev(0, false, 0xAF), Some(115)); // VolUp via vk
        assert_eq!(win_to_evdev(0x56, false, 0xE2), Some(86)); // 102ND
        assert_eq!(win_to_evdev(0x55, false, 0), None);
    }
}
