//! Where a Tonie's UID lands on the card, mirroring teddyCloud's own layout.
//!
//! teddyCloud (like the stock Toniebox) names a content file after the tag's
//! UID, byte-reversed and split in half: `getContentPathFromCharRUID` in its
//! `handler.c` does `osSprintf(filePath, "%.8s/%.8s", ruid, &ruid[8])`, where
//! `ruid` is the 16 hex characters of the reversed UID. The `/CACHE/` tree
//! uses the same layout.
//!
//! The two halves are returned as `u32`s; the firmware formats them as hex
//! when it opens the file.

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
/// `uid` is in the order the reader returns it (least significant byte
/// first, as ISO 15693 tags do, so a real UID starts with `0xE0` but its
/// content path *ends* in `E0`). The reversal matches teddyCloud's layout.
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

    /// A real Tonie, stored on a real card at `CONTENT/1C2D3E4F/500304E0`.
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

    /// A zero byte in the reversed UID must not be lost.
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
