//! Parses the update manifest the server publishes beside an image.
//!
//! Deliberately the same dull `key = value` format as `CONFIG.TXT`, for the
//! same reason: four fields do not justify a JSON parser with no allocator
//! behind it, and the person publishing an update already knows this format.
//!
//! Unlike `CONFIG.TXT` there is no comment rule to get wrong here — a `#` in a
//! version string or a path would be perverse — so `#` begins a comment only
//! when it is the first non-blank character of a line.

use crate::OtaError;
use heapless::String;

/// The manifest's name, relative to the server root.
pub const FILENAME: &str = "teddiebox.txt";

/// Longest version string accepted. `git describe --always --dirty` on this
/// repo produces well under half of this.
pub const MAX_VERSION: usize = 48;

/// Longest image path accepted, relative to the manifest.
pub const MAX_IMAGE_PATH: usize = 64;

/// Buffer the whole manifest is read into. Four lines need nothing like this
/// much; the slack is what lets `Truncated` mean something.
pub const MAX_MANIFEST: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Compared with our own for **difference**, never for order — so a
    /// deliberate downgrade works, which is how a bad build gets undone.
    pub version: String<MAX_VERSION>,
    pub sha256: [u8; 32],
    pub length: u32,
    /// Path to the image, relative to the manifest.
    pub image: String<MAX_IMAGE_PATH>,
}

impl Manifest {
    /// Parses a manifest that was read into a fixed buffer.
    ///
    /// `capacity` is how large that buffer was. **A read that filled it
    /// exactly is refused**, for the reason `Config::parse_read` gives: nothing
    /// distinguishes a file that just fits from one that was cut off, and a
    /// manifest cut mid-digest still parses into a plausible-looking value
    /// nobody published.
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
                // Unknown keys are ignored, so a manifest written for a newer
                // firmware is still readable by an older one. An empty
                // `version` or `image` value falls through here too, since
                // it supplies no value — the field stays unset and the
                // missing-field check below refuses it, rather than
                // accepting a value nobody actually gave.
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

    /// The digest below is typed out, not computed. A test that hashes the
    /// same bytes the parser does cannot disagree with the parser.
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
