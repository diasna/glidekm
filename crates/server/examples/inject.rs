//! Dev tool (Windows/Wine): injects a scripted sequence of mouse and keyboard
//! input with SendInput so the server's low-level hooks can be exercised
//! without a physical device. Usage: inject.exe

#[cfg(windows)]
fn main() {
    use std::thread::sleep;
    use std::time::Duration;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

    fn send(inputs: &[INPUT]) {
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        sleep(Duration::from_millis(30));
    }
    fn mouse(dx: i32, dy: i32, flags: u32, data: i32) -> INPUT {
        let mut i: INPUT = unsafe { std::mem::zeroed() };
        i.r#type = INPUT_MOUSE;
        i.Anonymous.mi = MOUSEINPUT {
            dx,
            dy,
            mouseData: data as u32,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        };
        i
    }
    fn key(scan: u16, up: bool) -> INPUT {
        let mut i: INPUT = unsafe { std::mem::zeroed() };
        i.r#type = INPUT_KEYBOARD;
        let vk = unsafe { MapVirtualKeyW(scan as u32, MAPVK_VSC_TO_VK) } as u16;
        i.Anonymous.ki = KEYBDINPUT {
            wVk: vk,
            wScan: scan,
            dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 },
            time: 0,
            dwExtraInfo: 0,
        };
        i
    }
    let step = |dx: i32, dy: i32, n: usize| {
        for _ in 0..n {
            send(&[mouse(dx, dy, MOUSEEVENTF_MOVE, 0)]);
        }
    };
    let tap = |scan: u16| {
        send(&[key(scan, false)]);
        send(&[key(scan, true)]);
    };

    unsafe { SetCursorPos(1800, 500) };
    sleep(Duration::from_millis(200));
    println!("inject: move right across the edge");
    step(10, 0, 15);
    println!("inject: move on client");
    step(5, 5, 4);
    println!("inject: type 'hi', click, scroll");
    tap(0x23);
    tap(0x17);
    send(&[mouse(0, 0, MOUSEEVENTF_LEFTDOWN, 0)]);
    send(&[mouse(0, 0, MOUSEEVENTF_LEFTUP, 0)]);
    send(&[mouse(0, 0, MOUSEEVENTF_WHEEL, -120)]);
    println!("inject: back left onto Windows");
    step(-10, 0, 15);
    println!("inject: hotkey ctrl+alt+shift+s (switch to client center), type 'a', hotkey again");
    let chord = [0x1Du16, 0x38, 0x2A];
    for s in chord {
        send(&[key(s, false)]);
    }
    tap(0x1F);
    for s in chord {
        send(&[key(s, true)]);
    }
    tap(0x1E);
    for s in chord {
        send(&[key(s, false)]);
    }
    tap(0x1F);
    for s in chord {
        send(&[key(s, true)]);
    }
    println!("inject: done");
}

#[cfg(not(windows))]
fn main() {
    eprintln!("inject is a Windows-only dev tool");
}
