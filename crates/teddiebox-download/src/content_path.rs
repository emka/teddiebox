//! Where a Tonie's UID lands on the card, mirroring teddyCloud's own layout.
//!
//! teddyCloud (and the stock Toniebox before it) names a content file after
//! the tag's UID, byte-reversed and split in half:
//! `getContentPathFromCharRUID` in `handler.c` does
//! `osSprintf(filePath, "%.8s/%.8s", ruid, &ruid[8])` where `ruid` is the
//! 16 hex characters of the UID with its bytes reversed
//! (`handler_cloud.c`'s `bswap_64` on the `strtoull`'d UID confirms the same
//! reversal independently). This crate's `/CACHE/` tree mirrors that
//! convention so a downloaded file sits exactly where a stock one would.
//!
//! The two halves come back as `u32`s rather than formatted strings: the
//! firmware already knows how to render a `u32` as eight upper-case hex
//! digits when it opens a content file, and keeping that formatting there
//! keeps it out of this `no_std` crate.

/// The two halves of a content path, each rendered as eight upper-case hex
/// digits by the firmware that opens the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentPath {
    /// Directory name.
    pub directory: u32,
    /// File name within it.
    pub file: u32,
}

/// Maps a tag's UID to where its content lives on the card.
///
/// `uid` is in the order the reader hands it over (least significant byte
/// first, as ISO 15693 tags do — which is why a real UID starts with
/// `0xE0` but its content path *ends* in `E0`). The reversal below is not
/// this crate's choice; it is teddyCloud's on-disk convention, reproduced
/// so a downloaded file is found by anything that expects a stock layout.
pub fn content_path(uid: [u8; 8]) -> ContentPath {
    let mut ruid = uid;
    ruid.reverse();
    ContentPath {
        directory: u32::from_be_bytes([ruid[0], ruid[1], ruid[2], ruid[3]]),
        file: u32::from_be_bytes([ruid[4], ruid[5], ruid[6], ruid[7]]),
    }
}

#[cfg(test)]
mod tests {
    use super::{content_path, ContentPath};

    /// A real Tonie on the project's own SD card, at `CONTENT/1C2D3E4F/500304E0`.
    /// A synthetic vector could pass by construction; this one was observed
    /// on the card, so it is the one that actually proves the mapping right.
    #[test]
    fn a_real_tonie_uid_maps_to_its_observed_card_path() {
        let uid = [0xE0, 0x04, 0x03, 0x50, 0x4F, 0x3E, 0x2D, 0x1C];
        assert_eq!(
            content_path(uid),
            ContentPath {
                directory: 0x1C2D3E4F,
                file: 0x500304E0,
            }
        );
    }

    #[test]
    fn each_byte_lands_in_the_reversed_position() {
        let uid = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(
            content_path(uid),
            ContentPath {
                directory: 0x08070605,
                file: 0x04030201,
            }
        );
    }

    /// A `u64`-based implementation that round-trips the UID through an
    /// integer would lose a leading zero byte in the reversed form; building
    /// each half from its own four bytes cannot.
    #[test]
    fn a_zero_byte_in_the_reversed_form_is_not_dropped() {
        let uid = [0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33];
        assert_eq!(
            content_path(uid),
            ContentPath {
                directory: 0x33221100,
                file: 0xDDCCBBAA,
            }
        );
    }
}
