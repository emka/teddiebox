//! Ogg page header parsing, restricted to what a TAF file contains.

use crate::{TafError, PAGE_SIZE};

const CAPTURE_PATTERN: &[u8; 4] = b"OggS";
const MIN_HEADER_LEN: usize = 27;

/// Not part of the public API: `Packets` (returned by [`OggPage::packets`])
/// terminates on an oversized packet rather than reporting it -- `None`
/// there is indistinguishable from "page finished". That's the right
/// behaviour for `TafReader::next_packet`, which layers its own
/// `TafError::NotAnOggPage` on top after seeing the page-level iterator end
/// early, but it would be a fail-open trap for any other caller reading
/// straight from `OggPage`: a corrupt or malicious page would look like a
/// page that simply ran out of packets. Keeping both `pub(crate)` removes
/// that surface instead of documenting around it.
///
/// `granule_position`, `is_continuation`, and `packets` (with `Packets`
/// itself) have no caller left inside the crate either: `TafReader` scans
/// lacing tables with its own inline copy of this same logic rather than
/// calling back into this iterator. They stay `#[allow(dead_code)]` as
/// tested, load-bearing-by-parity primitives -- `page.rs`'s own tests pin
/// their behaviour independently of `reader.rs`'s copy -- rather than being
/// deleted as part of a visibility-only change.
#[allow(dead_code)]
pub(crate) struct OggPage<'a> {
    page: &'a [u8; PAGE_SIZE],
    segment_count: usize,
    payload_start: usize,
}

impl<'a> OggPage<'a> {
    pub(crate) fn parse(page: &'a [u8; PAGE_SIZE]) -> Result<Self, TafError> {
        if &page[0..4] != CAPTURE_PATTERN {
            return Err(TafError::NotAnOggPage);
        }
        let segment_count = page[26] as usize;
        let payload_start = MIN_HEADER_LEN + segment_count;
        if payload_start > PAGE_SIZE {
            return Err(TafError::NotAnOggPage);
        }
        Ok(Self {
            page,
            segment_count,
            payload_start,
        })
    }

    /// Sample position at the end of this page, used for seeking and for
    /// reporting playback position.
    #[allow(dead_code)]
    pub(crate) fn granule_position(&self) -> u64 {
        u64::from_le_bytes(self.page[6..14].try_into().unwrap())
    }

    /// True when this page's first packet continues from the previous page.
    #[allow(dead_code)]
    pub(crate) fn is_continuation(&self) -> bool {
        self.page[5] & 0x01 != 0
    }

    /// Yields each complete packet in this page.
    ///
    /// A trailing packet whose lacing runs out before a terminating value
    /// (below 255) is not complete within this page — it continues onto the
    /// next one — and is silently dropped rather than handed to the caller
    /// as a truncated packet.
    #[allow(dead_code)]
    pub(crate) fn packets(&self) -> Packets<'a> {
        Packets {
            page: self.page,
            lacing: MIN_HEADER_LEN,
            lacing_end: MIN_HEADER_LEN + self.segment_count,
            payload: self.payload_start,
        }
    }
}

#[allow(dead_code)]
pub(crate) struct Packets<'a> {
    page: &'a [u8; PAGE_SIZE],
    lacing: usize,
    lacing_end: usize,
    payload: usize,
}

impl<'a> Iterator for Packets<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.lacing >= self.lacing_end {
            return None;
        }
        let start = self.payload;
        let mut len = 0usize;
        // A packet ends at the first lacing value below 255.
        loop {
            if self.lacing >= self.lacing_end {
                // Packet continues onto the next page; TAF pages are built so
                // this does not occur, so drop the fragment rather than
                // returning a truncated packet to the decoder.
                self.payload = start + len;
                return None;
            }
            let v = self.page[self.lacing] as usize;
            self.lacing += 1;
            len += v;
            if v < 255 {
                break;
            }
        }
        let end = start.checked_add(len)?;
        if end > PAGE_SIZE {
            // Fail closed, exactly as the continuation branch does. Returning
            // None without exhausting the lacing table would let a later call
            // resume from a stale `start` and hand the decoder an in-bounds
            // slice that is not a real packet.
            self.lacing = self.lacing_end;
            return None;
        }
        self.payload = end;
        Some(&self.page[start..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembles a minimal valid Ogg page carrying the given packets.
    fn ogg_page(packets: &[&[u8]]) -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(CAPTURE_PATTERN);

        let mut lacing = heapless::Vec::<u8, 255>::new();
        for p in packets {
            let mut remaining = p.len();
            while remaining >= 255 {
                lacing.push(255).unwrap();
                remaining -= 255;
            }
            lacing.push(remaining as u8).unwrap();
        }

        page[26] = lacing.len() as u8;
        page[27..27 + lacing.len()].copy_from_slice(&lacing);

        let mut off = 27 + lacing.len();
        for p in packets {
            page[off..off + p.len()].copy_from_slice(p);
            off += p.len();
        }
        page
    }

    #[test]
    fn rejects_a_page_without_the_capture_pattern() {
        let page = [0u8; PAGE_SIZE];
        assert!(matches!(OggPage::parse(&page), Err(TafError::NotAnOggPage)));
    }

    #[test]
    fn yields_a_single_packet() {
        let page = ogg_page(&[&[1, 2, 3, 4]]);
        let parsed = OggPage::parse(&page).unwrap();
        let packets: heapless::Vec<&[u8], 8> = parsed.packets().collect();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0], &[1, 2, 3, 4]);
    }

    #[test]
    fn yields_multiple_packets_in_order() {
        let page = ogg_page(&[&[0xAA; 3], &[0xBB; 7]]);
        let parsed = OggPage::parse(&page).unwrap();
        let packets: heapless::Vec<&[u8], 8> = parsed.packets().collect();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], &[0xAA; 3]);
        assert_eq!(packets[1], &[0xBB; 7]);
    }

    #[test]
    fn reads_the_granule_position() {
        let mut page = ogg_page(&[&[1]]);
        page[6..14].copy_from_slice(&960u64.to_le_bytes());
        let parsed = OggPage::parse(&page).unwrap();
        assert_eq!(parsed.granule_position(), 960);
    }

    #[test]
    fn yields_a_packet_of_exactly_255_bytes() {
        // Lacing [255, 0]: a length that is an exact multiple of 255 is
        // terminated by an explicit zero segment, not folded into the 255.
        let page = ogg_page(&[&[0xCC; 255]]);
        let parsed = OggPage::parse(&page).unwrap();
        let packets: heapless::Vec<&[u8], 8> = parsed.packets().collect();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0], &[0xCC; 255][..]);
    }

    #[test]
    fn drops_a_packet_whose_lacing_runs_out_without_a_terminator() {
        // A single lacing entry of 255 with no follow-up value below 255:
        // the packet is not complete within this page, so it must be
        // dropped rather than handed to the decoder as a truncated packet.
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(CAPTURE_PATTERN);
        page[26] = 1;
        page[27] = 255;
        let parsed = OggPage::parse(&page).unwrap();
        let mut packets = parsed.packets();
        assert_eq!(packets.next(), None);
        // Terminality: once the lacing table is exhausted, further calls
        // must keep returning None rather than re-reading stale state.
        assert_eq!(packets.next(), None);
    }

    #[test]
    fn stops_yielding_after_an_oversized_packet_even_with_lacing_entries_remaining() {
        // 20 segments: the first 17 (sixteen 255s plus a terminating 1)
        // declare a 4081-byte packet starting at payload offset 47, which
        // overruns the 4096-byte page. Three lacing entries remain after
        // it. A corrupt/malicious page must not let those remaining
        // entries be interpreted starting from a stale payload offset.
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(CAPTURE_PATTERN);
        let segment_count: usize = 20;
        page[26] = segment_count as u8;
        for i in 0..16 {
            page[27 + i] = 255;
        }
        page[27 + 16] = 1; // terminator: total declared length 16*255+1 = 4081
        page[27 + 17] = 10; // remaining lacing entries after the oversized packet
        page[27 + 18] = 0;
        page[27 + 19] = 0;

        let parsed = OggPage::parse(&page).unwrap();
        let mut packets = parsed.packets();
        assert_eq!(packets.next(), None);
        // Must not resume from a stale `start` and hand back a bogus
        // in-bounds slice built from the remaining lacing entries.
        assert_eq!(packets.next(), None);
    }

    #[test]
    fn is_continuation_reports_true_when_the_flag_is_set() {
        let mut page = ogg_page(&[&[1]]);
        page[5] |= 0x01;
        let parsed = OggPage::parse(&page).unwrap();
        assert!(parsed.is_continuation());
    }

    #[test]
    fn is_continuation_reports_false_when_the_flag_is_clear() {
        let page = ogg_page(&[&[1]]);
        let parsed = OggPage::parse(&page).unwrap();
        assert!(!parsed.is_continuation());
    }
}
