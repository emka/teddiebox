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
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use teddiebox_identity::{render, HEADER, MAX_BODY};

/// Reads `dir/upper`, falling back to `dir/upper.to_lowercase()`.
///
/// The files this tool reads come straight off the box's SD card, and a
/// Linux `vfat` mount presents 8.3 short names lowercased by default — so
/// the natural `cp /mnt/card/CERT/*.der $TEDDIEBOX_IDENTITY_DIR/` produces
/// `client.der`, not `CLIENT.DER`. Trying the documented spelling first and
/// the lowercase one second means that copy just works; naming both
/// spellings in the error means it is obvious why when it does not.
fn read_der(dir: &Path, upper: &str) -> Result<Vec<u8>, String> {
    let lower = upper.to_lowercase();
    if let Ok(bytes) = fs::read(dir.join(upper)) {
        return Ok(bytes);
    }
    match fs::read(dir.join(&lower)) {
        Ok(bytes) => Ok(bytes),
        Err(trouble) => Err(format!(
            "neither {upper} nor {lower} found in {} — {trouble}",
            dir.display()
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
