//! Ogg page structure, restricted to what a TAF file contains.
//!
//! The only code that walks an Ogg page's lacing table.
//!
//! The position is a plain value rather than an iterator, because
//! `TafReader` keeps it between calls and cannot hold an iterator that
//! borrows its own page buffer.

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
    /// `Ok(None)` means no page begins there. `toniefile` packs the small
    /// OpusHead and OpusTags pages into the same 4096-byte block as the start
    /// of the audio, so callers ask this repeatedly, and "nothing more in this
    /// block" is normal, not corruption.
    ///
    /// `Err` means a page begins there but does not fit the block. This must
    /// stay separate from `Ok(None)`, or a corrupt page would look like the
    /// end of the block.
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
            // Bytes 14..18, little-endian, inside the fixed header checked
            // above.
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
    /// Read here so only this module knows the page layout.
    pub(crate) fn serial(&self) -> u32 {
        self.serial
    }

    /// Offset where this page's remaining payload starts. Another page packed
    /// into the same block cannot start before this.
    pub(crate) fn payload_start(&self) -> usize {
        self.payload
    }

    /// Yields the next complete packet, advancing past it.
    ///
    /// `Ok(None)` means this page has no more complete packets. A last packet
    /// whose lacing has no terminator continues on the next page, so it is
    /// dropped rather than returned incomplete. Its bytes are still skipped,
    /// because another page may follow them.
    ///
    /// `Err` means the page declares a packet that overruns the block. The
    /// cursor is then left exhausted, so a caller that ignores the error
    /// cannot read garbage from a stale offset.
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
    use crate::test_pages::ogg_page;

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
        // Given
        let page = [0u8; PAGE_SIZE];

        // When
        let found = PacketCursor::at_page(&page, 0);

        // Then
        assert!(found.unwrap().is_none());
    }

    #[test]
    fn rejects_a_page_whose_segment_table_overruns_the_block() {
        // Given: a page header at the very end of the block, whose segment
        // table cannot fit, so this is corruption, not a missing page
        let mut page = [0u8; PAGE_SIZE];
        let offset = PAGE_SIZE - MIN_HEADER_LEN;
        page[offset..offset + 4].copy_from_slice(CAPTURE_PATTERN);
        page[offset + 26] = 1;

        // When
        let found = PacketCursor::at_page(&page, offset);

        // Then
        assert_eq!(found, Err(TafError::NotAnOggPage));
    }

    /// The limit is the block's end, so a segment table that reaches it
    /// exactly still fits.
    #[test]
    fn accepts_a_page_whose_segment_table_ends_at_the_block_end() {
        // Given: one lacing entry in the block's last byte
        let mut page = [0u8; PAGE_SIZE];
        let offset = PAGE_SIZE - MIN_HEADER_LEN - 1;
        page[offset..offset + 4].copy_from_slice(CAPTURE_PATTERN);
        page[offset + 26] = 1;

        // When
        let found = PacketCursor::at_page(&page, offset);

        // Then
        assert!(matches!(found, Ok(Some(_))), "{found:?}");
    }

    #[test]
    fn yields_a_single_packet() {
        // Given
        let page = ogg_page(&[&[1, 2, 3, 4]]);

        // When
        let packets = collect(&page, 0).unwrap();

        // Then
        assert_eq!(packets.as_slice(), &[&[1, 2, 3, 4][..]]);
    }

    #[test]
    fn yields_multiple_packets_in_order() {
        // Given
        let page = ogg_page(&[&[0xAA; 3], &[0xBB; 7]]);

        // When
        let packets = collect(&page, 0).unwrap();

        // Then
        assert_eq!(packets.as_slice(), &[&[0xAA; 3][..], &[0xBB; 7][..]]);
    }

    #[test]
    fn yields_a_packet_of_exactly_255_bytes() {
        // Given: lacing [255, 0], since a length that is an exact multiple of
        // 255 is terminated by an explicit zero segment, not folded into the
        // 255
        let page = ogg_page(&[&[0xCC; 255]]);

        // When
        let packets = collect(&page, 0).unwrap();

        // Then
        assert_eq!(packets.as_slice(), &[&[0xCC; 255][..]]);
    }

    #[test]
    fn finds_a_page_packed_in_behind_another() {
        // Given: as `toniefile` writes it, OpusHead's page is much smaller
        // than a block, and the next page starts right after it
        let mut page = ogg_page(&[b"first"]);
        let second = 27 + 1 + 5;
        page[second..second + 4].copy_from_slice(CAPTURE_PATTERN);
        page[second + 26] = 1;
        page[second + 27] = 6;
        page[second + 28..second + 34].copy_from_slice(b"second");

        // When
        let (from_first, from_second) =
            (collect(&page, 0).unwrap(), collect(&page, second).unwrap());

        // Then
        assert_eq!(from_first.as_slice(), &[&b"first"[..]]);
        assert_eq!(from_second.as_slice(), &[&b"second"[..]]);
    }

    #[test]
    fn drops_a_packet_whose_lacing_runs_out_without_a_terminator() {
        // Given: a single lacing entry of 255 with no value below 255 after
        // it, so the packet continues on the next page
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(CAPTURE_PATTERN);
        page[26] = 1;
        page[27] = 255;
        let mut cursor = PacketCursor::at_page(&page, 0).unwrap().unwrap();

        // When
        let packets = [cursor.next(&page), cursor.next(&page)];

        // Then: dropped rather than returned incomplete, and nothing after
        // the lacing table is used up
        assert_eq!(packets, [Ok(None), Ok(None)]);
    }

    #[test]
    fn steps_over_a_dropped_fragments_bytes_rather_than_stopping_on_them() {
        // Given: a dropped packet's 255 bytes still take up space
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(CAPTURE_PATTERN);
        page[26] = 1;
        page[27] = 255;
        let mut cursor = PacketCursor::at_page(&page, 0).unwrap().unwrap();

        // When
        let packet = cursor.next(&page);

        // Then: a page packed in after it starts after them
        assert_eq!(packet, Ok(None));
        assert_eq!(cursor.payload_start(), 28 + 255);
    }

    #[test]
    fn stops_yielding_after_an_oversized_packet_even_with_lacing_entries_remaining() {
        // Given: 20 segments, the first 17 (sixteen 255s and a 1) declaring a
        // 4081-byte packet at payload offset 47, which overruns the 4096-byte
        // page, then three more lacing entries
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(CAPTURE_PATTERN);
        page[26] = 20;
        for i in 0..16 {
            page[27 + i] = 255;
        }
        page[27 + 16] = 1;
        page[27 + 17] = 10;
        page[27 + 18] = 0;
        page[27 + 19] = 0;
        let mut cursor = PacketCursor::at_page(&page, 0).unwrap().unwrap();

        // When
        let packets = [cursor.next(&page), cursor.next(&page)];

        // Then: the entries after it are not read from a stale offset
        assert_eq!(packets, [Err(TafError::NotAnOggPage), Ok(None)]);
    }
}
