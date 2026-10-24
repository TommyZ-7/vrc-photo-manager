//! Generates an unencrypted minisign key pair for release signing.
//!
//! Usage:
//! `cargo run -p vrc-photo-update --example generate_update_key -- <secret-key-path>`
//!
//! The secret key is written with mode 0600. Only the public key line is
//! printed, so it can be appended to `packaging/update/keys.txt`.

use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use minisign::KeyPair;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: generate_update_key <secret-key-path>")?;

    let keypair = KeyPair::generate_unencrypted_keypair()?;
    let public_box = keypair.pk.to_box()?.to_string();
    let secret_box = keypair.sk.to_box(None)?.to_string();

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(secret_box.as_bytes())?;
    file.sync_all()?;

    let public_key = public_box
        .lines()
        .find(|line| line.starts_with("RW"))
        .ok_or("public key box did not contain a key line")?;
    println!("{public_key}");
    eprintln!("wrote secret key to {}", path.display());
    Ok(())
}
