//! The `.MET` file that says how long a download is meant to be.
//!
//! `embedded-sdmmc` cannot rename files, so a finished download cannot be
//! moved into place. Instead, the expected length is written before the first
//! body byte, and a content file shorter than that is incomplete. **A
//! downloaded file with no sidecar is always incomplete**, so a failed
//! sidecar write is safe.
//!
//! Plain text, like `CONFIG.TXT`, so it can be read on a laptop when
//! debugging.

use crate::DownloadError;
use core::fmt::Write as _;
use heapless::String;
use teddiebox_cloud::ETag;

/// Room for `length = 4294967295\n` plus `etag = ` and the longest ETag kept.
pub const MAX_SIDECAR: usize = 128;

/// The rendered sidecar must never be truncated. The longest possible one is
/// `length = ` (9) + u32::MAX (10 digits) + newline (1) + `etag = ` (7) +
/// MAX_ETAG (64) + newline (1) = 92 bytes.
#[allow(clippy::int_plus_one)]
const _: () = assert!(
    MAX_SIDECAR >= 9 + 10 + 1 + 7 + teddiebox_cloud::MAX_ETAG + 1,
    "MAX_SIDECAR must hold the longest possible sidecar render"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// How long the whole file is meant to be, from the server.
    pub length: u32,
    /// What to validate a resume against. Absent if the server sent none.
    pub etag: Option<ETag>,
}

impl Sidecar {
    pub fn render(&self) -> String<MAX_SIDECAR> {
        let mut out = String::new();
        // MAX_SIDECAR fits the longest possible content (checked above), so
        // neither write can fail.
        let _ = writeln!(out, "length = {}", self.length);
        if let Some(etag) = &self.etag {
            let _ = writeln!(out, "etag = {etag}");
        }
        out
    }

    pub fn parse(text: &str) -> Result<Self, DownloadError> {
        let mut length = None;
        let mut etag = None;

        for raw in text.lines() {
            // Split on the first '=' only: an ETag may contain more. A line
            // without one, such as a blank line, carries nothing; a comment's
            // key never names a field.
            let Some((key, value)) = raw.split_once('=') else {
                continue;
            };
            match key.trim() {
                "length" => {
                    length = Some(
                        value
                            .trim()
                            .parse()
                            .map_err(|_| DownloadError::MalformedSidecar)?,
                    )
                }
                "etag" => etag = ETag::try_from(value.trim()).ok(),
                // Ignore unknown keys, so newer firmware's files still work.
                _ => {}
            }
        }

        Ok(Self {
            length: length.ok_or(DownloadError::MalformedSidecar)?,
            etag,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teddiebox_cloud::{ETag, MAX_ETAG};

    #[test]
    fn a_rendered_sidecar_parses_back_to_what_it_held() {
        let etag = ETag::try_from("\"v1\"").unwrap();
        let original = Sidecar {
            length: 27841285,
            etag: Some(etag),
        };
        let text = original.render();
        assert_eq!(Sidecar::parse(&text).unwrap(), original);
    }

    /// People read this file when debugging, so the exact bytes are tested,
    /// not just the round trip.
    #[test]
    fn the_rendered_form_is_the_dullest_thing_that_works() {
        let etag = ETag::try_from("\"v1\"").unwrap();
        let text = Sidecar {
            length: 27841285,
            etag: Some(etag),
        }
        .render();
        assert_eq!(text.as_str(), "length = 27841285\netag = \"v1\"\n");
    }

    #[test]
    fn a_server_that_sent_no_etag_leaves_the_line_out() {
        let text = Sidecar {
            length: 12,
            etag: None,
        }
        .render();
        assert_eq!(text.as_str(), "length = 12\n");
        assert_eq!(Sidecar::parse(&text).unwrap().etag, None);
    }

    /// Without a length, completeness cannot be checked.
    #[test]
    fn a_sidecar_without_a_length_is_malformed() {
        assert_eq!(
            Sidecar::parse("etag = \"v1\"\n"),
            Err(DownloadError::MalformedSidecar)
        );
    }

    #[test]
    fn a_length_that_is_not_a_number_is_malformed() {
        assert_eq!(
            Sidecar::parse("length = banana\n"),
            Err(DownloadError::MalformedSidecar)
        );
    }

    #[test]
    fn unknown_keys_are_ignored_so_a_newer_box_does_not_break_an_older_one() {
        let parsed = Sidecar::parse("length = 12\nfuture = 7\n").unwrap();
        assert_eq!(parsed.length, 12);
    }

    #[test]
    fn blank_lines_and_comments_are_skipped() {
        // Given
        let text = "# written by the box\n\n#length = 99\nlength = 12\n   \n";

        // When
        let parsed = Sidecar::parse(text).unwrap();

        // Then
        assert_eq!(parsed.length, 12);
    }

    /// An etag may contain spaces and equals signs, so only the first `=`
    /// splits the line.
    #[test]
    fn an_etag_keeps_everything_after_the_first_equals() {
        let parsed = Sidecar::parse("length = 1\netag = \"a=b c\"\n").unwrap();
        assert_eq!(parsed.etag.as_deref(), Some("\"a=b c\""));
    }

    /// A sidecar with a 64-character etag still fits the buffer.
    #[test]
    fn the_longest_possible_sidecar_still_fits() {
        let etag = ETag::try_from("x".repeat(MAX_ETAG).as_str()).unwrap();
        let text = Sidecar {
            length: u32::MAX,
            etag: Some(etag),
        }
        .render();
        assert!(text.len() <= MAX_SIDECAR, "rendered {} bytes", text.len());
    }
}
