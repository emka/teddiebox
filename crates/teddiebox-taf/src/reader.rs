//! Sequential and chapter-addressed reading over a `PageSource`.

use crate::{OggPage, PageSource, TafError, TonieHeader, PAGE_SIZE};

/// Largest Opus packet we will hand to the decoder.
pub const MAX_PACKET: usize = 1275;

/// Bytes in a fixed Ogg page header, up to and including the segment count,
/// before the (variable-length) segment table. Mirrors `page::MIN_HEADER_LEN`,
/// which is private to that module.
const MIN_HEADER_LEN: usize = 27;

pub struct TafReader<S: PageSource> {
    source: S,
    header: TonieHeader,
    page_index: u32,
    page: [u8; PAGE_SIZE],
    /// Byte offset of the next packet within `page`, and the lacing cursor.
    packet_cursor: usize,
    lacing_cursor: usize,
    lacing_end: usize,
}

impl<S: PageSource> TafReader<S> {
    pub fn open(mut source: S) -> Result<Self, TafError> {
        if source.page_count() < 2 {
            return Err(TafError::MalformedHeader);
        }
        let mut page = [0u8; PAGE_SIZE];
        source
            .read_page(0, &mut page)
            .map_err(|_| TafError::MalformedHeader)?;
        let header = TonieHeader::parse(&page)?;

        let mut reader = Self {
            source,
            header,
            page_index: 0,
            page,
            packet_cursor: 0,
            lacing_cursor: 0,
            lacing_end: 0,
        };
        reader.load_page(1)?;
        Ok(reader)
    }

    pub fn header(&self) -> &TonieHeader {
        &self.header
    }

    pub fn chapter_count(&self) -> usize {
        self.header.chapter_pages.len()
    }

    fn load_page(&mut self, index: u32) -> Result<(), TafError> {
        if index >= self.source.page_count() {
            return Err(TafError::PageOutOfRange);
        }
        self.source
            .read_page(index, &mut self.page)
            .map_err(|_| TafError::PageOutOfRange)?;
        let parsed = OggPage::parse(&self.page)?;
        let segment_count = self.page[26] as usize;
        self.page_index = index;
        self.lacing_cursor = MIN_HEADER_LEN;
        self.lacing_end = MIN_HEADER_LEN + segment_count;
        self.packet_cursor = MIN_HEADER_LEN + segment_count;
        let _ = parsed;
        Ok(())
    }

    /// Positions the reader at the first packet of chapter `n` (zero-based).
    pub fn seek_to_chapter(&mut self, n: usize) -> Result<(), TafError> {
        let ogg_page = *self
            .header
            .chapter_pages
            .get(n)
            .ok_or(TafError::PageOutOfRange)?;
        // Chapter indices are Ogg-stream-relative; file page 0 is the header.
        // Without the +1, chapter 0 would load the header instead of audio.
        let file_page = ogg_page.checked_add(1).ok_or(TafError::PageOutOfRange)?;
        self.load_page(file_page)
    }

    /// Copies the next Opus packet into `out`, returning its length.
    /// `Ok(None)` means end of stream.
    pub fn next_packet(&mut self, out: &mut [u8]) -> Result<Option<usize>, TafError> {
        loop {
            if self.lacing_cursor < self.lacing_end {
                let start = self.packet_cursor;
                let mut len = 0usize;
                let mut complete = false;
                while self.lacing_cursor < self.lacing_end {
                    let v = self.page[self.lacing_cursor] as usize;
                    self.lacing_cursor += 1;
                    len += v;
                    if v < 255 {
                        complete = true;
                        break;
                    }
                }
                if complete {
                    let end = start + len;
                    if end > PAGE_SIZE || len > out.len() {
                        return Err(TafError::NotAnOggPage);
                    }
                    self.packet_cursor = end;
                    out[..len].copy_from_slice(&self.page[start..end]);
                    return Ok(Some(len));
                }
            }

            // This Ogg page's lacing table is exhausted. A single
            // PAGE_SIZE-byte container block can hold more than one real
            // Ogg page back to back: `toniefile` packs the tiny OpusHead
            // and OpusTags pages together with the start of the audio
            // stream rather than leaving the rest of the block empty. Look
            // for another real Ogg page immediately following the one we
            // just finished, still inside the buffered block, before
            // reading a new block from the source.
            //
            // Without this, the reader silently drops every packet packed
            // behind the first page in a block instead of erroring: it
            // looks like a clean end-of-page and just advances past them.
            let candidate = self.packet_cursor;
            if candidate + MIN_HEADER_LEN <= PAGE_SIZE
                && self.page[candidate..candidate + 4] == *b"OggS"
            {
                let segment_count = self.page[candidate + 26] as usize;
                let lacing_start = candidate + MIN_HEADER_LEN;
                let lacing_end = lacing_start + segment_count;
                if lacing_end > PAGE_SIZE {
                    return Err(TafError::NotAnOggPage);
                }
                self.lacing_cursor = lacing_start;
                self.lacing_end = lacing_end;
                self.packet_cursor = lacing_end;
                continue;
            }

            // No further real Ogg page in this block: advance to the next
            // container block, or report end of stream.
            let next = self.page_index + 1;
            if next >= self.source.page_count() {
                return Ok(None);
            }
            self.load_page(next)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SlicePages;

    const FIXTURE: &[u8] = include_bytes!("../tests/data/sine.taf");

    #[test]
    fn opens_the_real_fixture() {
        let r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        assert_eq!(r.header().audio_id, 0x1234_5678);
    }

    #[test]
    fn reads_packets_until_the_stream_ends() {
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let mut count = 0usize;
        while r.next_packet(&mut buf).unwrap().is_some() {
            count += 1;
        }
        // The fixture is 5 s of audio (granule 239_040 at 48 kHz) encoded as
        // 83 packets of ~60 ms each, plus the two header packets: 85 total,
        // exactly. Verified independently by walking the raw file bytes.
        // `> 200` (assuming 20 ms frames) is not achievable against this
        // fixture at any packet count -- see task-5-report.md.
        assert_eq!(count, 85, "expected the fixture's exact packet count");
    }

    #[test]
    fn the_first_two_packets_are_the_opus_headers() {
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..8], b"OpusHead");

        let n2 = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..8], b"OpusTags");
        assert!(n > 0 && n2 > 0);
    }

    #[test]
    fn the_fixture_has_one_chapter_starting_at_the_first_ogg_page() {
        let r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        assert_eq!(r.chapter_count(), 1);
        // Ogg-stream-relative: 0 is the first Ogg page, at file page 1.
        assert_eq!(r.header().chapter_pages.as_slice(), &[0]);
    }

    #[test]
    fn seeking_to_chapter_zero_lands_on_audio_not_the_header() {
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        r.seek_to_chapter(0).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let n = r.next_packet(&mut buf).unwrap().expect("a packet");
        // The header page is not an Ogg page, so landing on it would fail to
        // parse rather than yield a packet. Guard the off-by-one explicitly.
        assert!(n > 0);
        assert_ne!(
            &buf[..n.min(4)],
            b"OggS",
            "packet payload must not be a raw page header"
        );
    }

    #[test]
    fn seeking_past_the_last_chapter_is_an_error() {
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let n = r.chapter_count();
        assert_eq!(r.seek_to_chapter(n), Err(TafError::PageOutOfRange));
    }
}
