//! glidekm-release: key generation and release-manifest signing.
//!
//!   glidekm-release keygen SECRET_FILE PUBLIC_FILE
//!   glidekm-release manifest VERSION FILE...        > glidekm-release.txt
//!   glidekm-release sign MANIFEST                   (secret hex in $GLIDEKM_SIGNING_KEY)
//!   glidekm-release verify MANIFEST SIG PUBLIC_FILE

use ed25519_dalek::{Signer, SigningKey};
use glidekm_core::update::{self, Manifest};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn read(p: &str) -> Result<Vec<u8>, String> {
    std::fs::read(p).map_err(|e| format!("{p}: {e}"))
}

fn run(a: &[String]) -> Result<(), String> {
    match a.first().map(String::as_str) {
        Some("keygen") if a.len() == 3 => {
            let seed = glidekm_core::session::random_bytes();
            let sk = SigningKey::from_bytes(&seed);
            glidekm_core::ipc::write_private(Path::new(&a[1]), &format!("{}\n", update::hex(&seed)))
                .map_err(|e| e.to_string())?;
            std::fs::write(&a[2], format!("{}\n", update::hex(&sk.verifying_key().to_bytes())))
                .map_err(|e| e.to_string())?;
            println!("secret key -> {} (keep private)\npublic key -> {}", a[1], a[2]);
            Ok(())
        }
        Some("manifest") if a.len() >= 3 => {
            let mut files = Vec::new();
            for p in &a[2..] {
                let name = Path::new(p).file_name().unwrap().to_string_lossy().to_string();
                files.push((name, update::sha256(&read(p)?)));
            }
            print!("{}", Manifest { version: a[1].trim_start_matches('v').to_string(), files }.to_text());
            Ok(())
        }
        Some("sign") if a.len() == 2 => {
            let secret = std::env::var("GLIDEKM_SIGNING_KEY").map_err(|_| "GLIDEKM_SIGNING_KEY is not set")?;
            let seed = update::hex32(&secret).ok_or("GLIDEKM_SIGNING_KEY must be 64 hex characters")?;
            let sk = SigningKey::from_bytes(&seed);
            let manifest = read(&a[1])?;
            let sig = update::hex(&sk.sign(&manifest).to_bytes());
            std::fs::write(format!("{}.sig", a[1]), format!("{sig}\n")).map_err(|e| e.to_string())?;
            // Self-check against the public key compiled into the apps.
            match update::public_key() {
                Some(pk) => {
                    update::verify(&manifest, &sig, &pk)
                        .map_err(|e| format!("{e} - the secret does not match crates/core/release-key.pub"))?;
                    println!("signed {} (matches the apps' public key)", a[1]);
                }
                None => println!("signed {} (warning: apps have no public key compiled in)", a[1]),
            }
            Ok(())
        }
        Some("verify") if a.len() == 4 => {
            let pk = update::hex32(&String::from_utf8_lossy(&read(&a[3])?)).ok_or("bad public key")?;
            let m = update::verify(&read(&a[1])?, &String::from_utf8_lossy(&read(&a[2])?), &pk)?;
            println!("OK: version {} with {} files", m.version, m.files.len());
            Ok(())
        }
        _ => Err("usage: keygen SECRET PUBLIC | manifest VERSION FILE... | sign MANIFEST | verify MANIFEST SIG PUBLIC".into()),
    }
}
