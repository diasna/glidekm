//! Raw /dev/uinput virtual devices (no external crates beyond libc).

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
const INPUT_PROP_POINTER: u32 = 0x00;
const BUS_USB: u16 = 0x03;

/// Absolute axis range used by the absolute pointer (resolution independent).
pub const ABS_MAX: i32 = 32767;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

const fn io(nr: u32) -> u32 {
    ((b'U' as u32) << 8) | nr
}
const fn iow(nr: u32, size: usize) -> u32 {
    (1 << 30) | ((size as u32) << 16) | ((b'U' as u32) << 8) | nr
}
const UI_DEV_CREATE: u32 = io(1);
const UI_DEV_DESTROY: u32 = io(2);
const UI_DEV_SETUP: u32 = iow(3, std::mem::size_of::<UinputSetup>());
const UI_ABS_SETUP: u32 = iow(4, std::mem::size_of::<UinputAbsSetup>());
const UI_SET_EVBIT: u32 = iow(100, 4);
const UI_SET_KEYBIT: u32 = iow(101, 4);
const UI_SET_RELBIT: u32 = iow(102, 4);
const UI_SET_ABSBIT: u32 = iow(103, 4);
const UI_SET_PROPBIT: u32 = iow(110, 4);

pub struct Device {
    fd: RawFd,
    buf: Vec<InputEvent>,
}

pub struct Spec<'a> {
    pub name: &'a str,
    pub product: u16,
    pub keys: &'a [u16],
    pub rels: &'a [u16],
    pub abs: bool,
    pub pointer: bool,
}

unsafe fn ioctl_int(fd: RawFd, req: u32, v: libc::c_int) -> io::Result<()> {
    // glibc takes c_ulong, musl takes c_int: `as _` covers both.
    if libc::ioctl(fd, req as _, v) < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

unsafe fn ioctl_ptr<T>(fd: RawFd, req: u32, p: *const T) -> io::Result<()> {
    if libc::ioctl(fd, req as _, p) < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Device {
    pub fn create(spec: &Spec) -> io::Result<Device> {
        let path = CString::new("/dev/uinput").unwrap();
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let dev = Device {
            fd,
            buf: Vec::with_capacity(16),
        };
        unsafe {
            if !spec.keys.is_empty() {
                ioctl_int(fd, UI_SET_EVBIT, EV_KEY as _)?;
                for &k in spec.keys {
                    ioctl_int(fd, UI_SET_KEYBIT, k as _)?;
                }
            }
            if !spec.rels.is_empty() {
                ioctl_int(fd, UI_SET_EVBIT, EV_REL as _)?;
                for &r in spec.rels {
                    ioctl_int(fd, UI_SET_RELBIT, r as _)?;
                }
            }
            if spec.abs {
                ioctl_int(fd, UI_SET_EVBIT, EV_ABS as _)?;
                for code in [ABS_X, ABS_Y] {
                    ioctl_int(fd, UI_SET_ABSBIT, code as _)?;
                    let s = UinputAbsSetup {
                        code,
                        absinfo: InputAbsinfo {
                            value: 0,
                            minimum: 0,
                            maximum: ABS_MAX,
                            fuzz: 0,
                            flat: 0,
                            resolution: 0,
                        },
                    };
                    ioctl_ptr(fd, UI_ABS_SETUP, &s)?;
                }
            }
            if spec.pointer {
                ioctl_int(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER as _)?;
            }
            let mut setup = UinputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    vendor: 0x1209,
                    product: spec.product,
                    version: 1,
                },
                name: [0; 80],
                ff_effects_max: 0,
            };
            let n = spec.name.len().min(79);
            setup.name[..n].copy_from_slice(&spec.name.as_bytes()[..n]);
            ioctl_ptr(fd, UI_DEV_SETUP, &setup)?;
            ioctl_int(fd, UI_DEV_CREATE as _, 0)?;
        }
        Ok(dev)
    }

    pub fn push(&mut self, type_: u16, code: u16, value: i32) {
        self.buf.push(InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        });
    }

    /// Append SYN_REPORT and write everything queued.
    pub fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        self.push(EV_SYN, SYN_REPORT, 0);
        let bytes = self.buf.len() * std::mem::size_of::<InputEvent>();
        let r = unsafe { libc::write(self.fd, self.buf.as_ptr() as *const libc::c_void, bytes) };
        if r < 0 {
            glidekm_core::warn!("uinput write failed: {}", io::Error::last_os_error());
        }
        self.buf.clear();
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd, UI_DEV_DESTROY as _, 0);
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn struct_sizes_match_kernel_abi() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
        // Known values from <linux/uinput.h> on x86_64.
        assert_eq!(UI_DEV_SETUP, 0x405c5503);
        assert_eq!(UI_ABS_SETUP, 0x401c5504);
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_DEV_CREATE, 0x5501);
    }
}
