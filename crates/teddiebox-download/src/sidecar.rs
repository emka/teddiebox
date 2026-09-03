//! The `.MET` file that says how long a download is meant to be.
//!
//! `embedded-sdmmc` has no rename, so a download cannot be committed by moving
//! it into place. This is the substitute: the expected length is written
//! before the first body byte, and a content file whose length has not reached
//! it is incomplete. **A content file with no sidecar beside it is incomplete
//! by definition**, which is what makes a failed sidecar write safe rather
//! than silently poisonous.
//!
//! Text, not a packed struct, for the same reason `teddiebox.conf` is text:
//! it will be read on a laptop with a card reader on an evening when
//! something has already gone wrong.

use crate::DownloadError;
use core::fmt::Write as _;
use heapless::String;
use teddiebox_cloud::ETag;

/// Room for `length = 4294967295\n` plus `etag = ` and the longest ETag kept.
pub const MAX_SIDECAR: usize = 128;

/// The render must not silently truncate the very file that vouches for a
/// download's completeness. The longest possible render is `length = ` (9) +
/// u32::MAX (10 digits) + newline (1) + `etag = ` (7) + MAX_ETAG (64) + newline (1) = 92 bytes.
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
        // Both writes are bounded by MAX_SIDECAR, which the tests pin against
        // the longest possible content, so neither can fail in practice.
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
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split on the first '=' only: an ETag may contain more.
            let Some((key, value)) = line.split_once('=') else {
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
                // Ignored so a newer box's file does not break an older one.
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

    /// The format is read by a human with a card reader when a download has
    /// gone wrong, so it is pinned to exact bytes rather than to a round trip.
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

    /// Without a length there is no completeness test, so a sidecar missing
    /// one is worse than useless — it would vouch for a file it cannot check.
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

    /// An etag may contain spaces and equals signs. Splitting on the first
    /// `=` and keeping the rest verbatim is what preserves them.
    #[test]
    fn an_etag_keeps_everything_after_the_first_equals() {
        let parsed = Sidecar::parse("length = 1\netag = \"a=b c\"\n").unwrap();
        assert_eq!(parsed.etag.as_deref(), Some("\"a=b c\""));
    }

    /// A file 64 characters of etag long still has to render into the buffer
    /// it will be written from.
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
