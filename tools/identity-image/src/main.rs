//! Builds the `cert` partition's image from the box's certificate and key.
//!
//! Reads `$TEDDIEBOX_IDENTITY_DIR/CLIENT.DER` and `PRIVATE.DER` and writes one
//! file for `espflash write-bin`. The directory is an environment variable
//! rather than an argument, so the private key's path is not typed each time
//! or left in shell history.
//!
//! **It prints lengths, never contents**, like the firmware's console does
//! for the tag token and the SLIX password.
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use teddiebox_identity::{render, HEADER, MAX_BODY};

/// Reads `dir/upper`, falling back to `dir/upper.to_lowercase()`.
///
/// The files come from the box's SD card, and Linux shows 8.3 short names in
/// lower case by default, so `cp /mnt/card/CERT/*.der
/// $TEDDIEBOX_IDENTITY_DIR/` gives `client.der`, not `CLIENT.DER`. Both
/// spellings are tried, and both are named in the error.
fn read_der(dir: &Path, upper: &str) -> Result<Vec<u8>, String> {
    let lower = upper.to_lowercase();
    let bytes = match fs::read(dir.join(upper)) {
        Ok(bytes) => bytes,
        Err(_) => fs::read(dir.join(&lower)).map_err(|trouble| {
            format!(
                "neither {upper} nor {lower} found in {} — {trouble}",
                dir.display()
            )
        })?,
    };

    // Every DER object here is a SEQUENCE, so its first byte is 0x30. This
    // catches a PEM instead of a DER, a text file, or the wrong directory,
    // which would otherwise only show up as a failed TLS handshake on the box.
    match bytes.first() {
        None => Err(format!("{upper} is empty")),
        Some(0x30) => Ok(bytes),
        Some(first) => Err(format!(
            "{upper} does not look like DER — it starts {first:#04x}, not 0x30 (SEQUENCE), \
             and is {} bytes. A PEM file starts with `-----BEGIN` (0x2d).",
            bytes.len()
        )),
    }
}

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

    let certificate = match read_der(&dir, "CLIENT.DER") {
        Ok(bytes) => bytes,
        Err(trouble) => {
            eprintln!("identity-image: cannot read the certificate — {trouble}");
            return ExitCode::FAILURE;
        }
    };
    let key = match read_der(&dir, "PRIVATE.DER") {
        Ok(bytes) => bytes,
        Err(trouble) => {
            eprintln!("identity-image: cannot read the key — {trouble}");
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
