//! Self-update from GitHub Releases, verified with Ed25519.
//!
//! Each release carries `glidekm-release.txt` (version + SHA-256 of every
//! raw binary) and `glidekm-release.txt.sig` (hex Ed25519 signature made in
//! CI with the private key stored as a GitHub secret). The public key is
//! compiled in (`release-key.pub`); nothing is installed unless the manifest
//! signature verifies and the downloaded file's hash matches.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub const MANIFEST: &str = "glidekm-release.txt";
pub const SIGNATURE: &str = "glidekm-release.txt.sig";
pub const ASSET_SERVER_WIN: &str = "glidekm-server.exe";
pub const ASSET_CLIENT_LINUX: &str = "glidekm-client-linux-x86_64";
pub const ASSET_SETTINGS_LINUX: &str = "glidekm-settings-linux-x86_64";

const DEFAULT_BASE: &str = "https://github.com/diasna/glidekm/releases/latest/download";
const PUBLIC_KEY: &str = include_str!("../release-key.pub");

/// Where release files are downloaded from (override for testing with
/// `GLIDEKM_UPDATE_BASE`).
pub fn base_url() -> String {
    std::env::var("GLIDEKM_UPDATE_BASE").unwrap_or_else(|_| DEFAULT_BASE.into())
}

pub fn public_key() -> Option<[u8; 32]> {
    hex32(PUBLIC_KEY.trim())
}

/// Updates are only possible when a release key is compiled in.
pub fn enabled() -> bool {
    public_key().is_some()
}

pub fn hex32(s: &str) -> Option<[u8; 32]> {
    let v = unhex(s)?;
    v.try_into().ok()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub files: Vec<(String, [u8; 32])>,
}

impl Manifest {
    pub fn to_text(&self) -> String {
        let mut s = format!("version = {}\n", self.version);
        for (name, h) in &self.files {
            s += &format!("sha256 {} = {}\n", name, hex(h));
        }
        s
    }

    pub fn parse(text: &str) -> Result<Manifest, String> {
        let mut version = None;
        let mut files = Vec::new();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let (k, v) = line.split_once('=').ok_or("bad manifest line")?;
            let (k, v) = (k.trim(), v.trim());
            if k == "version" {
                version = Some(v.to_string());
            } else if let Some(name) = k.strip_prefix("sha256 ") {
                files.push((name.trim().to_string(), hex32(v).ok_or("bad hash")?));
            }
        }
        Ok(Manifest {
            version: version.ok_or("manifest has no version")?,
            files,
        })
    }

    pub fn hash_of(&self, name: &str) -> Option<[u8; 32]> {
        self.files.iter().find(|(n, _)| n == name).map(|(_, h)| *h)
    }
}

/// Verify `sig_hex` over `manifest` with `key`, then parse it.
pub fn verify(manifest: &[u8], sig_hex: &str, key: &[u8; 32]) -> Result<Manifest, String> {
    let key = VerifyingKey::from_bytes(key).map_err(|_| "invalid release key")?;
    let sig: [u8; 64] = unhex(sig_hex)
        .and_then(|v| v.try_into().ok())
        .ok_or("malformed signature")?;
    key.verify(manifest, &Signature::from_bytes(&sig))
        .map_err(|_| "release signature does not verify")?;
    Manifest::parse(std::str::from_utf8(manifest).map_err(|_| "manifest not UTF-8")?)
}

/// "0.10.2" > "0.9.9"; non-numeric parts compare as 0.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(candidate) > parse(current)
}

/// Download transport (WinHTTP on Windows, curl on Linux, fakes in tests).
pub trait Fetch {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Fetch and verify the latest manifest. Ok(None) = up to date.
pub fn check(f: &dyn Fetch, current: &str) -> Result<Option<Manifest>, String> {
    let key = public_key().ok_or("updates are not enabled in this build (no release key)")?;
    let base = base_url();
    let manifest = f.get(&format!("{base}/{MANIFEST}"))?;
    let sig = f.get(&format!("{base}/{SIGNATURE}"))?;
    let m = verify(&manifest, &String::from_utf8_lossy(&sig), &key)?;
    Ok(newer(&m.version, current).then_some(m))
}

/// Download one release file and check it against the signed manifest.
pub fn download(f: &dyn Fetch, m: &Manifest, name: &str) -> Result<Vec<u8>, String> {
    let want = m
        .hash_of(name)
        .ok_or_else(|| format!("{name} is not part of release {}", m.version))?;
    let data = f.get(&format!("{}/{name}", base_url()))?;
    if sha256(&data) != want {
        return Err(format!("{name}: checksum mismatch, not installing"));
    }
    Ok(data)
}

/// Curl-based transport (Linux).
pub struct Curl;

impl Fetch for Curl {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "120", "--retry", "2", url])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(format!(
                "download failed ({}): {}",
                url.rsplit('/').next().unwrap_or(url),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::HashMap;

    struct Fake(HashMap<String, Vec<u8>>);
    impl Fetch for Fake {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            let name = url.rsplit('/').next().unwrap();
            self.0.get(name).cloned().ok_or_else(|| "404".into())
        }
    }

    fn release(sk: &SigningKey, version: &str, bin: &[u8]) -> Fake {
        let m = Manifest {
            version: version.into(),
            files: vec![(ASSET_SERVER_WIN.into(), sha256(bin))],
        };
        let text = m.to_text();
        let sig = hex(&sk.sign(text.as_bytes()).to_bytes());
        Fake(HashMap::from([
            (MANIFEST.to_string(), text.into_bytes()),
            (SIGNATURE.to_string(), sig.into_bytes()),
            (ASSET_SERVER_WIN.to_string(), bin.to_vec()),
        ]))
    }

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("v0.10.0", "0.9.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn verify_and_download() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let fake = release(&sk, "9.9.9", b"new exe");
        let text = fake.0[MANIFEST].clone();
        let sig = String::from_utf8(fake.0[SIGNATURE].clone()).unwrap();
        let m = verify(&text, &sig, &pk).unwrap();
        assert_eq!(m.version, "9.9.9");
        assert_eq!(download(&fake, &m, ASSET_SERVER_WIN).unwrap(), b"new exe");

        // tampered manifest
        let mut bad = text.clone();
        bad[11] ^= 1;
        assert!(verify(&bad, &sig, &pk).is_err());
        // wrong key
        let other = SigningKey::from_bytes(&[8u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(verify(&text, &sig, &other).is_err());
        // swapped binary
        let mut evil = fake;
        evil.0.insert(ASSET_SERVER_WIN.into(), b"evil".to_vec());
        assert!(download(&evil, &m, ASSET_SERVER_WIN).is_err());
    }

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            version: "1.2.3".into(),
            files: vec![("a".into(), [1; 32]), ("b-c".into(), [2; 32])],
        };
        assert_eq!(Manifest::parse(&m.to_text()).unwrap(), m);
    }
}
