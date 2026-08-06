//! Ogg page structure, restricted to what a TAF file contains.
//!
//! This is the only implementation of the lacing walk. It used to be two —
//! one here and one inlined into `TafReader::next_packet` — which agreed
//! only because their tests agreed, and had already drifted on what to do
//! with a dropped fragment.
//!
//! The position is a value rather than an iterator because `TafReader` has
//! to keep it between calls, and it cannot hold an iterator borrowing its
//! own page buffer.

use crate::{TafError, PAGE_SIZE};

const CAPTURE_PATTERN: &[u8; 4] = b"OggS";

/// Bytes of fixed Ogg page header before the segment table.
const MIN_HEADER_LEN: usize = 27;

/// A position within one container block: how far through a page's lacing
/// table, and where the next packet's bytes start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PacketCursor {
    lacing: usize,
    lacing_end: usize,
    payload: usize,
    serial: u32,
}

impl PacketCursor {
    /// A cursor with nothing left to yield, for a reader that has not
    /// positioned itself on a page yet.
    pub(crate) const EMPTY: Self = Self {
        lacing: 0,
        lacing_end: 0,
        payload: 0,
        serial: 0,
    };

    /// Positions at the first packet of the Ogg page beginning at `offset`.
    ///
    /// `Ok(None)` means no page begins there. `toniefile` packs the tiny
    /// OpusHead and OpusTags pages into the same 4096-byte block as the
    /// start of the audio, so a caller walking a block has to ask this
    /// question repeatedly, and "nothing more in this block" is a normal
    /// answer rather than corruption.
    ///
    /// `Err` is reserved for a page that does begin there and then does not
    /// fit the block. That distinction has to survive: folding it into
    /// `Ok(None)` would let a corrupt page read as a finished one, which
    /// fails open.
    pub(crate) fn at_page(page: &[u8; PAGE_SIZE], offset: usize) -> Result<Option<Self>, TafError> {
        if offset + MIN_HEADER_LEN > PAGE_SIZE || page[offset..offset + 4] != *CAPTURE_PATTERN {
            return Ok(None);
        }
        let lacing = offset + MIN_HEADER_LEN;
        let lacing_end = lacing + page[offset + 26] as usize;
        if lacing_end > PAGE_SIZE {
            return Err(TafError::NotAnOggPage);
        }
        Ok(Some(Self {
            lacing,
            lacing_end,
            payload: lacing_end,
            // Bytes 14..18, little-endian, well inside the fixed header the
            // bounds check above already guaranteed.
            serial: u32::from_le_bytes([
                page[offset + 14],
                page[offset + 15],
                page[offset + 16],
                page[offset + 17],
            ]),
        }))
    }

    /// Which Ogg stream this page claims to belong to.
    ///
    /// Read here rather than by the reader poking at raw offsets, so that
    /// the page layout stays knowledge of this module alone.
    pub(crate) fn serial(&self) -> u32 {
        self.serial
    }

    /// Offset where this page's remaining payload starts, and so the
    /// earliest point another page could be packed in behind it.
    pub(crate) fn payload_start(&self) -> usize {
        self.payload
    }

    /// Yields the next complete packet, advancing past it.
    ///
    /// `Ok(None)` means this page has no more complete packets. That covers
    /// a trailing packet whose lacing runs out without a terminator: it
    /// continues onto a following page, so it is dropped rather than handed
    /// on truncated — but its declared bytes are still stepped over, because
    /// they occupy the payload and anything packed in behind starts after
    /// them.
    ///
    /// `Err` means the page declares a packet that overruns the block. The
    /// cursor is left exhausted so that a caller which ignores the error
    /// cannot resume from a stale offset and manufacture an in-bounds slice
    /// that is not a packet.
    pub(crate) fn next<'a>(
        &mut self,
        page: &'a [u8; PAGE_SIZE],
    ) -> Result<Option<&'a [u8]>, TafError> {
        if self.lacing >= self.lacing_end {
            return Ok(None);
        }

        let start = self.payload;
        let mut len = 0usize;
        let mut complete = false;
        // A packet ends at the first lacing value below 255.
        while self.lacing < self.lacing_end {
            let v = page[self.lacing] as usize;
            self.lacing += 1;
            len += v;
            if v < 255 {
                complete = true;
                break;
            }
        }

        if !complete {
            self.payload = (start + len).min(PAGE_SIZE);
            return Ok(None);
        }

        let end = start + len;
        if end > PAGE_SIZE {
            self.lacing = self.lacing_end;
            return Err(TafError::NotAnOggPage);
        }

        self.payload = end;
        Ok(Some(&page[start..end]))
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

    /// Every packet the cursor yields from `offset`, or the error it stops on.
    fn collect(page: &[u8; PAGE_SIZE], offset: usize) -> Result<heapless::Vec<&[u8], 8>, TafError> {
        let mut cursor = PacketCursor::at_page(page, offset).unwrap().unwrap();
        let mut out = heapless::Vec::new();
        while let Some(p) = cursor.next(page)? {
            out.push(p).unwrap();
        }
        Ok(out)
    }

    #[test]
    fn reports_no_page_where_the_capture_pattern_is_absent() {
        let page = [0u8; PAGE_SIZE];
        assert!(PacketCursor::at_page(&page, 0).unwrap().is_none());
    }

    #[test]
    fn rejects_a_page_whose_segment_table_overruns_the_block() {
        // A page header that starts 10 bytes before the end of the block:
        // its segment table cannot fit, so this is corruption rather than
        // an absent page.
        let mut page = [0u8; PAGE_SIZE];
        let offset = PAGE_SIZE - MIN_HEADER_LEN;
        page[offset..offset + 4].copy_from_slice(CAPTURE_PATTERN);
        page[offset + 26] = 1;
        assert_eq!(
            PacketCursor::at_page(&page, offset),
            Err(TafError::NotAnOggPage)
        );
    }

    #[test]
    fn yields_a_single_packet() {
        let page = ogg_page(&[&[1, 2, 3, 4]]);
        assert_eq!(collect(&page, 0).unwrap().as_slice(), &[&[1, 2, 3, 4][..]]);
    }

    #[test]
    fn yields_multiple_packets_in_order() {
        let page = ogg_page(&[&[0xAA; 3], &[0xBB; 7]]);
        let packets = collect(&page, 0).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], &[0xAA; 3]);
        assert_eq!(packets[1], &[0xBB; 7]);
    }

    #[test]
    fn yields_a_packet_of_exactly_255_bytes() {
        // Lacing [255, 0]: a length that is an exact multiple of 255 is
        // terminated by an explicit zero segment, not folded into the 255.
        let page = ogg_page(&[&[0xCC; 255]]);
        assert_eq!(collect(&page, 0).unwrap().as_slice(), &[&[0xCC; 255][..]]);
    }

    #[test]
    fn finds_a_page_packed_in_behind_another() {
        // What `toniefile` actually writes: OpusHead's page is far smaller
        // than a block, and the next page starts immediately after it.
        let mut page = ogg_page(&[b"first"]);
        let second = 27 + 1 + 5;
        page[second..second + 4].copy_from_slice(CAPTURE_PATTERN);
        page[second + 26] = 1;
        page[second + 27] = 6;
        page[second + 28..second + 34].copy_from_slice(b"second");

        assert_eq!(collect(&page, 0).unwrap().as_slice(), &[&b"first"[..]]);
        assert_eq!(
            collect(&page, second).unwrap().as_slice(),
            &[&b"second"[..]]
        );
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

        let mut cursor = PacketCursor::at_page(&page, 0).unwrap().unwrap();
        assert_eq!(cursor.next(&page), Ok(None));
        // Terminality: once the lacing table is exhausted, further calls
        // must keep returning None rather than re-reading stale state.
        assert_eq!(cursor.next(&page), Ok(None));
    }

    #[test]
    fn steps_over_a_dropped_fragments_bytes_rather_than_stopping_on_them() {
        // The dropped packet's 255 bytes are declared and occupy the
        // payload. A page packed in behind it starts after them, so a
        // cursor that stops at the fragment's start hides it.
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(CAPTURE_PATTERN);
        page[26] = 1;
        page[27] = 255;

        let mut cursor = PacketCursor::at_page(&page, 0).unwrap().unwrap();
        assert_eq!(cursor.next(&page), Ok(None));
        assert_eq!(cursor.payload_start(), 28 + 255);
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
        page[26] = 20;
        for i in 0..16 {
            page[27 + i] = 255;
        }
        page[27 + 16] = 1; // terminator: total declared length 16*255+1 = 4081
        page[27 + 17] = 10; // remaining lacing entries after the oversized packet
        page[27 + 18] = 0;
        page[27 + 19] = 0;

        let mut cursor = PacketCursor::at_page(&page, 0).unwrap().unwrap();
        assert_eq!(cursor.next(&page), Err(TafError::NotAnOggPage));
        // Must not resume from a stale `start` and hand back a bogus
        // in-bounds slice built from the remaining lacing entries.
        assert_eq!(cursor.next(&page), Ok(None));
    }
}
