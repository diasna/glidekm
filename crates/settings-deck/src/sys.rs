//! System helpers for glidekm Settings: addresses, the systemd user service,
//! starting the client, uinput permission fix, log tailing.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const UNIT: &str = "glidekm-client.service";

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Non-loopback IPv4 addresses, most useful first (private LAN ranges).
pub fn local_ipv4s() -> Vec<String> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) == 0 {
            let mut p = ifap;
            while !p.is_null() {
                let a = (*p).ifa_addr;
                if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                    let sin = &*(a as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    if !ip.is_loopback() && !out.contains(&ip) {
                        out.push(ip);
                    }
                }
                p = (*p).ifa_next;
            }
            libc::freeifaddrs(ifap);
        }
    }
    out.sort_by_key(|ip| !ip.is_private());
    out.iter().map(|i| i.to_string()).collect()
}

fn quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn unit_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("systemd/user").join(UNIT))
}

fn unit_installed() -> bool {
    unit_path().is_some_and(|p| p.exists())
}

/// Some(enabled) when installed as a systemd user service.
pub fn autostart() -> Option<bool> {
    if !unit_installed() {
        return None;
    }
    let out = Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim() == "enabled")
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    if quiet(
        "systemctl",
        &["--user", if on { "enable" } else { "disable" }, UNIT],
    ) {
        Ok(())
    } else {
        Err("systemctl failed".into())
    }
}

/// Start the background client: the systemd service if installed, else the
/// glidekm-client binary next to this one (or on PATH).
pub fn start_client() -> Result<&'static str, String> {
    if unit_installed() && quiet("systemctl", &["--user", "start", UNIT]) {
        return Ok("service");
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("glidekm-client")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("glidekm-client"));
    Command::new(sibling)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| "background process")
        .map_err(|e| format!("cannot start glidekm-client: {e}"))
}

pub fn has_pkexec() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("pkexec").exists()))
}

/// Install the uinput udev rule via pkexec (graphical password prompt).
pub fn fix_uinput() -> Result<(), String> {
    let script = "printf '%s\\n' 'KERNEL==\"uinput\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"' > /etc/udev/rules.d/60-glidekm-uinput.rules && modprobe uinput; udevadm control --reload-rules && udevadm trigger --name-match=uinput";
    if quiet("pkexec", &["sh", "-c", script]) {
        Ok(())
    } else {
        Err("Not changed (cancelled or no permission). Use the command in Konsole instead.".into())
    }
}

pub fn generate_passphrase() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    glidekm_core::session::random_bytes()
        .iter()
        .take(20)
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

/// Last `n` lines of a (small) text file.
pub fn tail(p: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Ask the background client to check for (and install) updates now.
pub fn request_update_check() -> Result<(), String> {
    glidekm_core::ipc::write_private(&glidekm_core::ipc::update_request_path(), "1")
        .map_err(|e| format!("cannot reach glidekm: {e}"))
}

/// Re-run this program (after an update replaced the binary).
pub fn restart_self() -> ! {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let exe = PathBuf::from(
        exe.to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_string(),
    );
    let err = Command::new(exe).args(std::env::args().skip(1)).exec();
    eprintln!("restart failed: {err}");
    std::process::exit(1)
}
