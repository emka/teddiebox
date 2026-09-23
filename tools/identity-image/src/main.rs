//! Builds the `cert` partition's image from the box's certificate and key.
//!
//! Reads `$TEDDIEBOX_IDENTITY_DIR/CLIENT.DER` and `PRIVATE.DER` and writes one
//! file for `espflash write-bin`. The directory is an environment variable
//! rather than an argument so the paths to a private key are not retyped —
//! and not left in shell history — on every box.
//!
//! **It prints lengths and never contents.** The same rule the firmware's
//! console follows for the tag token and the SLIX password.
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use teddiebox_identity::{render, HEADER, MAX_BODY};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(out_path) = args.next() else {
        eprintln!("identity-image: usage: identity-image <output path>");
        return ExitCode::FAILURE;
    };

    let Ok(dir) = env::var("TEDDIEBOX_IDENTITY_DIR") else {
        eprintln!(
            "identity-image: TEDDIEBOX_IDENTITY_DIR is not set.\n\
             Set it in .envrc.local to the directory holding CLIENT.DER and PRIVATE.DER.\n\
             It is deliberately not a path in this repository: those files are secrets."
        );
        return ExitCode::FAILURE;
    };
    let dir = PathBuf::from(dir);

    let certificate = match fs::read(dir.join("CLIENT.DER")) {
        Ok(bytes) => bytes,
        Err(trouble) => {
            eprintln!("identity-image: cannot read CLIENT.DER — {trouble}");
            return ExitCode::FAILURE;
        }
    };
    let key = match fs::read(dir.join("PRIVATE.DER")) {
        Ok(bytes) => bytes,
        Err(trouble) => {
            eprintln!("identity-image: cannot read PRIVATE.DER — {trouble}");
            return ExitCode::FAILURE;
        }
    };

    let mut image = vec![0u8; HEADER + MAX_BODY * 2];
    let written = match render(&certificate, &key, &mut image) {
        Ok(written) => written,
        Err(trouble) => {
            eprintln!("identity-image: cannot build the image — {trouble:?}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(trouble) = fs::write(&out_path, &image[..written]) {
        eprintln!("identity-image: cannot write {out_path} — {trouble}");
        return ExitCode::FAILURE;
    }

    // Lengths, never contents.
    println!(
        "identity-image: certificate {} bytes, key {} bytes, image {written} bytes -> {out_path}",
        certificate.len(),
        key.len()
    );
    ExitCode::SUCCESS
}
