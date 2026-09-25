//! Parses the update manifest the server publishes beside an image.
//!
//! The same simple `key = value` format as `CONFIG.TXT`: four fields do not
//! need JSON.
//!
//! Unlike `CONFIG.TXT`, `#` only starts a comment at the beginning of a line.

use crate::OtaError;
use heapless::String;

/// The manifest's name, relative to the server root.
pub const FILENAME: &str = "teddiebox.txt";

/// Longest version string accepted.
///
/// The ESP-IDF application descriptor's `version: [c_char; 32]` field
/// (esp-bootloader-esp-idf-0.6.0/src/lib.rs:141), minus a NUL. A longer
/// version would be silently cut short in the image. `git describe --always
/// --dirty` is well under this.
pub const MAX_VERSION: usize = 31;

/// Longest image path accepted, relative to the manifest.
pub const MAX_IMAGE_PATH: usize = 64;

/// Buffer the whole manifest is read into. Much larger than four lines need,
/// so a full buffer really means the file was too long.
pub const MAX_MANIFEST: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Compared with our own for **difference**, not order, so a downgrade
    /// works.
    pub version: String<MAX_VERSION>,
    pub sha256: [u8; 32],
    pub length: u32,
    /// Path to the image, relative to the manifest.
    pub image: String<MAX_IMAGE_PATH>,
}

impl Manifest {
    /// Parses a manifest that was read into a fixed buffer.
    ///
    /// `capacity` is the size of that buffer. **A read that filled it exactly
    /// is refused**, as in `Config::parse_read`: the file may have been cut
    /// off.
    pub fn parse_read(raw: &[u8], capacity: usize) -> Result<Self, OtaError> {
        if raw.len() >= capacity {
            return Err(OtaError::Truncated);
        }
        let text = core::str::from_utf8(raw).map_err(|_| OtaError::NotText)?;

        let mut version: Option<String<MAX_VERSION>> = None;
        let mut sha256: Option<[u8; 32]> = None;
        let mut length: Option<u32> = None;
        let mut image: Option<String<MAX_IMAGE_PATH>> = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or(OtaError::MalformedLine)?;
            let key = key.trim();
            let value = value.trim();

            match key {
                "version" if !value.is_empty() => {
                    version = Some(String::try_from(value).map_err(|_| OtaError::ValueTooLong)?);
                }
                "sha256" => sha256 = Some(crate::digest::parse_hex32(value)?),
                "length" => {
                    length = Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| OtaError::MalformedLength)?,
                    );
                }
                "image" if !value.is_empty() => {
                    image = Some(String::try_from(value).map_err(|_| OtaError::ValueTooLong)?);
                }
                // Ignore unknown keys, so older firmware can read a newer
                // manifest. An empty `version` or `image` also ends up here,
                // so it counts as missing below.
                _ => {}
            }
        }

        Ok(Manifest {
            version: version.ok_or(OtaError::MissingVersion)?,
            sha256: sha256.ok_or(OtaError::MissingSha256)?,
            length: length.ok_or(OtaError::MissingLength)?,
            image: image.ok_or(OtaError::MissingImage)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest is written out, not computed, so the test can disagree with
    /// the parser.
    const GOOD: &str = "\
version = 2026-09-15-a1b2c3d
sha256  = 3f786850e387550fdab836ed7e6dc881de23001b000000000000000000000000
length  = 1103728
image   = teddiebox.bin
";

    #[test]
    fn parses_every_field_of_a_well_formed_manifest() {
        let m = Manifest::parse_read(GOOD.as_bytes(), MAX_MANIFEST).unwrap();
        assert_eq!(m.version.as_str(), "2026-09-15-a1b2c3d");
        assert_eq!(m.length, 1_103_728);
        assert_eq!(m.image.as_str(), "teddiebox.bin");
        assert_eq!(
            m.sha256,
            [
                0x3f, 0x78, 0x68, 0x50, 0xe3, 0x87, 0x55, 0x0f, 0xda, 0xb8, 0x36, 0xed, 0x7e, 0x6d,
                0xc8, 0x81, 0xde, 0x23, 0x00, 0x1b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00,
            ]
        );
    }
}
