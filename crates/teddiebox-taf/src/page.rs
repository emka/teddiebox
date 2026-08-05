//! Ogg page header parsing, restricted to what a TAF file contains.

use crate::{TafError, PAGE_SIZE};

const CAPTURE_PATTERN: &[u8; 4] = b"OggS";
const MIN_HEADER_LEN: usize = 27;

pub struct OggPage<'a> {
    page: &'a [u8; PAGE_SIZE],
    segment_count: usize,
    payload_start: usize,
}

impl<'a> OggPage<'a> {
    pub fn parse(page: &'a [u8; PAGE_SIZE]) -> Result<Self, TafError> {
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
    pub fn granule_position(&self) -> u64 {
        u64::from_le_bytes(self.page[6..14].try_into().unwrap())
    }

    /// True when this page's first packet continues from the previous page.
    pub fn is_continuation(&self) -> bool {
        self.page[5] & 0x01 != 0
    }

    /// Yields each complete packet in this page.
    pub fn packets(&self) -> Packets<'a> {
        Packets {
            page: self.page,
            lacing: MIN_HEADER_LEN,
            lacing_end: MIN_HEADER_LEN + self.segment_count,
            payload: self.payload_start,
        }
    }
}

pub struct Packets<'a> {
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
}
