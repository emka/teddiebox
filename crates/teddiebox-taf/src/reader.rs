//! Sequential and chapter-addressed reading over a `PageSource`.

use crate::{PacketCursor, PageSource, TafError, TonieHeader, PAGE_SIZE};
use heapless::Vec;

/// Buffer size that can hold any packet a TAF file can contain.
///
/// Based on the container, not the codec. Every Ogg page in a TAF file fits
/// in one [`PAGE_SIZE`] block, and a packet lies entirely within its page, so
/// no packet can be larger than `PAGE_SIZE`, whatever the bitrate.
///
/// Not RFC 6716's 1275-byte maximum: that is the limit for one Opus *frame*,
/// and TAF's 60 ms packets hold several frames. Real Toniebox files have
/// packets up to 4053 bytes.
///
/// A packet larger than the caller's buffer returns
/// `TafError::BufferTooSmall` and is not consumed, so a retry with a bigger
/// buffer gets the same packet.
pub const MAX_PACKET: usize = PAGE_SIZE;

/// Reads a TAF file one [`PAGE_SIZE`] block at a time.
///
/// The block buffer is borrowed from the caller rather than held inline, so
/// the reader itself is small. On the box a reader lives inside nested async
/// functions, and each level copies its value between stack frames; a block
/// held inline was copied at every level and overflowed the stack.
pub struct TafReader<'p, S: PageSource> {
    source: S,
    header: TonieHeader,
    /// Index of the buffered 4096-byte *block*, not of an Ogg page: one block
    /// can hold several Ogg pages, which `next_packet` walks without changing
    /// this.
    page_index: u32,
    page: &'p mut [u8; PAGE_SIZE],
    /// Where the next packet starts within `page`. A value rather than an
    /// iterator, because it is kept between calls and this struct cannot
    /// borrow its own `page`.
    cursor: PacketCursor,
    /// File-page index of the last page that the header's `data_length`
    /// declares as part of the stream. Pages after it (such as padding) may
    /// exist in `source` but are not audio; `next_packet` treats them as the
    /// end of the stream.
    last_usable_page: u32,
    /// Serial of this file's Ogg stream, taken from the first page loaded.
    /// `None` only during `open`, before that page is read.
    ///
    /// The first page's serial must equal the header's `audio_id`, or
    /// `check_stream` rejects it. In real files every page's serial equals
    /// `audio_id` (checked on three commercial files, 22,110 pages), so a
    /// page that disagrees does not belong to this file.
    stream_serial: Option<u32>,
}

impl<'p, S: PageSource> TafReader<'p, S> {
    /// Opens `source`, reading blocks into `page`.
    pub fn open(source: S, page: &'p mut [u8; PAGE_SIZE]) -> Result<Self, TafError> {
        if source.page_count() < 2 {
            return Err(TafError::MalformedHeader);
        }

        // Built before the header is known, so page 0 is read straight into
        // the borrowed `page`, the only block buffer the reader uses.
        let mut reader = Self {
            source,
            header: TonieHeader {
                audio_id: 0,
                data_length: 0,
                chapter_pages: Vec::new(),
            },
            page_index: 0,
            page,
            cursor: PacketCursor::EMPTY,
            last_usable_page: 0,
            stream_serial: None,
        };
        reader
            .source
            .read_page(0, reader.page)
            .map_err(|_| TafError::Io)?;
        reader.header = TonieHeader::parse(reader.page)?;

        // The header declares how many bytes of Ogg stream follow it. Fewer
        // pages means the file was cut short; playing it would silently stop
        // the story early.
        let declared_pages = reader.header.data_length.div_ceil(PAGE_SIZE as u32);
        let available_pages = reader.source.page_count() - 1;
        if available_pages < declared_pages {
            return Err(TafError::TruncatedFile);
        }
        // A header declaring zero bytes of stream leaves no page to open:
        // file page 1 (OpusHead) would be past `last_usable_page`. This is a
        // fault in the header itself, so it is `MalformedHeader`, not
        // `TruncatedFile`.
        if declared_pages == 0 {
            return Err(TafError::MalformedHeader);
        }
        reader.last_usable_page = declared_pages;

        reader.load_page(1)?;
        Ok(reader)
    }

    pub fn header(&self) -> &TonieHeader {
        &self.header
    }

    pub fn chapter_count(&self) -> usize {
        self.header.chapter_pages.len()
    }

    /// Which chapter the reader is in, zero-based.
    ///
    /// Computed from the current position, because a story played straight
    /// through moves into later chapters without any seek.
    ///
    /// The answer is the last chapter start at or before the current page,
    /// using the same page conversion as `seek_to_chapter`. It is only
    /// meaningful if the chapter starts increase, which the format does not
    /// guarantee.
    pub fn current_chapter(&self) -> usize {
        self.header
            .chapter_pages
            .iter()
            .rposition(|&ogg_page| ogg_page.saturating_add(1) <= self.page_index)
            .unwrap_or(0)
    }

    fn load_page(&mut self, index: u32) -> Result<(), TafError> {
        if index >= self.source.page_count() {
            return Err(TafError::PageOutOfRange);
        }
        // Read straight into `self.page` rather than a separate candidate
        // buffer: on `Err` it may hold a partial or invalid page, but every
        // caller stops the whole read on any `Err` here and never touches the
        // reader again, so nothing depends on the old page surviving a failed
        // load. A candidate buffer would be a second block on the stack.
        //
        // The index is in range, so a failure here is a storage error.
        self.source
            .read_page(index, self.page)
            .map_err(|_| TafError::Io)?;
        let cursor = PacketCursor::at_page(self.page, 0)?.ok_or(TafError::NotAnOggPage)?;
        self.check_stream(cursor.serial())?;

        self.page_index = index;
        self.cursor = cursor;
        Ok(())
    }

    /// Accepts the first page's stream if it matches the header's
    /// `audio_id`, and rejects any later page from a different stream.
    ///
    /// Called wherever the reader moves onto a page, including a page packed
    /// behind another in the same block.
    fn check_stream(&mut self, serial: u32) -> Result<(), TafError> {
        match self.stream_serial {
            None if serial == self.header.audio_id => {
                self.stream_serial = Some(serial);
                Ok(())
            }
            None => Err(TafError::WrongStream),
            Some(expected) if serial == expected => Ok(()),
            Some(_) => Err(TafError::WrongStream),
        }
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
        // Also check against `last_usable_page`: a page after the declared
        // stream may still be a valid Ogg page left over from an older,
        // longer recording, and must not be played.
        if file_page > self.last_usable_page {
            return Err(TafError::PageOutOfRange);
        }
        self.load_page(file_page)
    }

    /// Which container block the reader is on.
    ///
    /// This is what the saved position stores. A block index rather than an
    /// Ogg page index, because the reader loads whole blocks.
    pub fn current_page(&self) -> u32 {
        self.page_index
    }

    /// Positions the reader at a block it reported earlier.
    ///
    /// Checked like [`seek_to_chapter`](Self::seek_to_chapter): a block past
    /// `last_usable_page` is not part of the stream, and file page 0 is the
    /// header, not audio.
    ///
    /// If the block starts in the middle of a packet, that packet is skipped,
    /// losing a few tens of milliseconds of audio. This is not noticeable.
    pub fn seek_to_page(&mut self, page: u32) -> Result<(), TafError> {
        if page == 0 || page > self.last_usable_page {
            return Err(TafError::PageOutOfRange);
        }
        self.load_page(page)
    }

    /// Copies the next Opus packet into `out`, returning its length.
    /// `Ok(None)` means end of stream.
    pub fn next_packet(&mut self, out: &mut [u8]) -> Result<Option<usize>, TafError> {
        loop {
            // Both failures below restore the saved position: after
            // `BufferTooSmall` a retry must get the same packet, and after a
            // corrupt page a retry must not read garbage from a half-advanced
            // cursor.
            let saved = self.cursor;
            match self.cursor.next(self.page) {
                Ok(Some(packet)) => {
                    let len = packet.len();
                    if len > out.len() {
                        self.cursor = saved;
                        return Err(TafError::BufferTooSmall);
                    }
                    out[..len].copy_from_slice(packet);
                    return Ok(Some(len));
                }
                Ok(None) => {}
                Err(e) => {
                    self.cursor = saved;
                    return Err(e);
                }
            }

            // This Ogg page is finished. One block can hold several Ogg pages
            // in a row: `toniefile` packs the small OpusHead and OpusTags
            // pages together with the start of the audio. Look for another
            // page in this block before reading the next block, or those
            // packets would be skipped.
            if let Some(nested) = PacketCursor::at_page(self.page, self.cursor.payload_start())? {
                self.check_stream(nested.serial())?;
                self.cursor = nested;
                continue;
            }

            // No more Ogg pages in this block: move to the next block, or
            // report the end of the stream. Stop at `last_usable_page`, not
            // at the end of the source, so trailing padding is not read as
            // audio.
            let next = self.page_index + 1;
            if next > self.last_usable_page {
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
        let mut page = [0u8; PAGE_SIZE];
        let r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        assert_eq!(r.header().audio_id, 0x1234_5678);
    }

    #[test]
    fn reads_packets_until_the_stream_ends() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let mut count = 0usize;
        while r.next_packet(&mut buf).unwrap().is_some() {
            count += 1;
        }
        // The fixture is 5 s of audio (granule 239_040 at 48 kHz) in 83
        // packets of about 60 ms, plus the two header packets: 85 in total,
        // checked by walking the raw file bytes.
        assert_eq!(count, 85, "expected the fixture's exact packet count");
    }

    #[test]
    fn the_first_two_packets_are_the_opus_headers() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..8], b"OpusHead");

        let n2 = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..8], b"OpusTags");
        assert!(n > 0 && n2 > 0);
    }

    #[test]
    fn the_fixture_has_one_chapter_starting_at_the_first_ogg_page() {
        let mut page = [0u8; PAGE_SIZE];
        let r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        assert_eq!(r.chapter_count(), 1);
        // Ogg-stream-relative: 0 is the first Ogg page, at file page 1.
        assert_eq!(r.header().chapter_pages.as_slice(), &[0]);
    }

    /// Checks that a packet's payload is returned, not a raw page. (The first
    /// packet of chapter 0 is `OpusHead`, not audio.)
    #[test]
    fn seeking_to_chapter_zero_returns_packet_payload_not_a_raw_unparsed_page() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        r.seek_to_chapter(0).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let n = r.next_packet(&mut buf).unwrap().expect("a packet");
        // Without the `+1` in `seek_to_chapter`, the header page would load
        // and fail, and the `unwrap` above would panic. This assertion checks
        // that the page header and lacing table were skipped.
        assert!(n > 0);
        assert_ne!(
            &buf[..n.min(4)],
            b"OggS",
            "packet payload must not be a raw page header"
        );
    }

    #[test]
    fn seeking_past_the_last_chapter_is_an_error() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        let n = r.chapter_count();
        assert_eq!(r.seek_to_chapter(n), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn reads_a_packet_as_large_as_a_page_can_hold() {
        let mut page = [0u8; PAGE_SIZE];
        // 4053 bytes is the largest packet seen in a real Toniebox file. The
        // fixtures' packets are at most 719 bytes, so this uses a synthetic
        // page.
        //
        // data_length (field 2) = 4096: one Ogg page of stream.
        let packet_len = 4053usize;
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[0x10, 0x80, 0x20]));
        file[PAGE_SIZE..].copy_from_slice(&ogg_page(&[&[0xAB; 4053]]));

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(n, packet_len);
        assert!(buf[..n].iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn a_packet_too_large_for_the_buffer_can_be_retried_with_a_bigger_one() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();

        // The OpusHead packet is 19 bytes, so 4 bytes is too small.
        let mut too_small = [0u8; 4];
        assert_eq!(r.next_packet(&mut too_small), Err(TafError::BufferTooSmall));

        // The failed attempt did not consume the packet: a retry gets the
        // same packet.
        let mut big_enough = [0u8; MAX_PACKET];
        let n = r.next_packet(&mut big_enough).unwrap().unwrap();
        assert_eq!(&big_enough[..n.min(8)], b"OpusHead");
    }

    /// Builds a minimal TAF header page with the given protobuf field bytes.
    /// A copy of the private helper in `header.rs`.
    fn header_page(fields: &[u8]) -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(&(fields.len() as u32).to_be_bytes());
        page[4..4 + fields.len()].copy_from_slice(fields);
        page
    }

    /// Builds a minimal valid Ogg page with the given packets. A copy of the
    /// private helper in `page.rs`.
    fn ogg_page(packets: &[&[u8]]) -> [u8; PAGE_SIZE] {
        ogg_page_with_serial(0, packets)
    }

    /// Like `ogg_page`, but with a stream serial.
    fn ogg_page_with_serial(serial: u32, packets: &[&[u8]]) -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(b"OggS");
        page[14..18].copy_from_slice(&serial.to_le_bytes());

        let mut off = 27usize;
        for p in packets {
            let mut remaining = p.len();
            while remaining >= 255 {
                page[off] = 255;
                off += 1;
                remaining -= 255;
            }
            page[off] = remaining as u8;
            off += 1;
        }
        let segment_count = off - 27;
        page[26] = segment_count as u8;

        for p in packets {
            page[off..off + p.len()].copy_from_slice(p);
            off += p.len();
        }
        page
    }

    #[test]
    fn a_first_page_whose_serial_disagrees_with_the_header_is_rejected() {
        let mut page = [0u8; PAGE_SIZE];
        // In real files every page's serial equals the header's audio_id, so
        // a first page that disagrees is not part of this file.
        //
        // data_length (field 2) = 4096 = one page; audio_id (field 3) =
        // 0xAAAA. The page has a different serial, 0xBBBB.
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE]
            .copy_from_slice(&header_page(&[0x10, 0x80, 0x20, 0x18, 0xAA, 0xD5, 0x02]));
        file[PAGE_SIZE..].copy_from_slice(&ogg_page_with_serial(0xBBBB, &[b"AAAA"]));

        assert_eq!(
            TafReader::open(SlicePages::new(&file).unwrap(), &mut page).err(),
            Some(TafError::WrongStream)
        );
    }

    #[test]
    fn a_block_belonging_to_another_stream_is_rejected_rather_than_decoded() {
        let mut page = [0u8; PAGE_SIZE];
        // After an interrupted write, page 2 is a valid Ogg page left over
        // from an older, longer recording. Only its stream serial shows it
        // does not belong. Playing it would play the end of the old story.
        //
        // data_length (field 2) = 8192, declaring both pages as stream;
        // audio_id (field 3) = 0xAAAA, matching the first page, so only the
        // second page is wrong.
        let mut file = [0u8; PAGE_SIZE * 3];
        file[0..PAGE_SIZE]
            .copy_from_slice(&header_page(&[0x10, 0x80, 0x40, 0x18, 0xAA, 0xD5, 0x02]));
        file[PAGE_SIZE..PAGE_SIZE * 2].copy_from_slice(&ogg_page_with_serial(0xAAAA, &[b"AAAA"]));
        file[PAGE_SIZE * 2..].copy_from_slice(&ogg_page_with_serial(0xBBBB, &[b"BBBB"]));

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"AAAA");
        assert_eq!(r.next_packet(&mut buf), Err(TafError::WrongStream));
    }

    #[test]
    fn a_page_packed_behind_another_is_checked_against_the_stream_too() {
        let mut page = [0u8; PAGE_SIZE];
        // The same leftover page, but packed behind a good page in the same
        // block. This is a different code path, so it has its own test.
        //
        // audio_id (field 3) = 0xAAAA, matching the block's first page.
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE]
            .copy_from_slice(&header_page(&[0x10, 0x80, 0x20, 0x18, 0xAA, 0xD5, 0x02]));

        let block = &mut file[PAGE_SIZE..];
        block[..PAGE_SIZE].copy_from_slice(&ogg_page_with_serial(0xAAAA, &[b"AAAA"]));

        // A second page immediately behind the first one's payload.
        const SECOND: usize = 27 + 1 + 4;
        let foreign = ogg_page_with_serial(0xBBBB, &[b"BBBB"]);
        block[SECOND..SECOND + 32].copy_from_slice(&foreign[..32]);

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"AAAA");
        assert_eq!(r.next_packet(&mut buf), Err(TafError::WrongStream));
    }

    #[test]
    fn seeking_onto_a_block_from_another_stream_is_rejected() {
        let mut page = [0u8; PAGE_SIZE];
        // A chapter can point at a leftover page from an older recording
        // inside the declared range, so `last_usable_page` does not catch
        // it; the stream serial does.
        //
        // data_length = 8192; chapter_pages (field 4, packed) = [0, 1];
        // audio_id (field 3) = 0xAAAA, matching the first page's serial.
        let mut file = [0u8; PAGE_SIZE * 3];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[
            0x10, 0x80, 0x40, 0x18, 0xAA, 0xD5, 0x02, 0x22, 0x02, 0x00, 0x01,
        ]));
        file[PAGE_SIZE..PAGE_SIZE * 2].copy_from_slice(&ogg_page_with_serial(0xAAAA, &[b"AAAA"]));
        file[PAGE_SIZE * 2..].copy_from_slice(&ogg_page_with_serial(0xBBBB, &[b"BBBB"]));

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut page).unwrap();

        assert_eq!(r.seek_to_chapter(1), Err(TafError::WrongStream));
    }

    #[test]
    fn a_failed_seek_still_reports_the_right_error() {
        let mut page = [0u8; PAGE_SIZE];
        // Chapter 0 at ogg page 0 (file page 1, holding two packets);
        // chapter 1 at ogg page 1 (file page 2, deliberately not a real Ogg
        // page at all).
        //
        // data_length (field 2) = 8192 = two pages, so chapter 1 fails
        // because page 2 is not an Ogg page, not because it is out of range.
        //
        // `load_page` reads straight into `self.page` (no separate
        // candidate buffer — see its own doc comment), so a failed seek no
        // longer guarantees the reader is still positioned where it was;
        // this crate's one caller always stops on any `Err` here rather
        // than reading on, and this test only pins the error itself.
        let mut file = [0u8; PAGE_SIZE * 3];
        file[0..PAGE_SIZE]
            .copy_from_slice(&header_page(&[0x10, 0x80, 0x40, 0x22, 0x02, 0x00, 0x01]));
        file[PAGE_SIZE..PAGE_SIZE * 2].copy_from_slice(&ogg_page(&[b"AAAA", b"BBBB"]));
        // file[PAGE_SIZE * 2..] is left all zero: not "OggS", not a page.

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"AAAA");

        assert_eq!(
            r.seek_to_chapter(1),
            Err(TafError::NotAnOggPage),
            "chapter 1 points at a page that isn't a real Ogg page"
        );
    }

    #[test]
    fn an_oversized_packet_does_not_leave_stale_state_for_a_retry() {
        let mut block = [0u8; PAGE_SIZE];
        // data_length (field 2) = 4096 = one page. audio_id is 0, which
        // matches the page's serial (also 0).
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[0x10, 0x80, 0x20]));

        // 20 segments: the first 17 (sixteen 255s and a 1) declare a
        // 4081-byte packet at payload offset 47, which overruns the 4096-byte
        // page. Three lacing entries follow it (as in the test in `page.rs`).
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(b"OggS");
        page[26] = 20;
        for i in 0..16 {
            page[27 + i] = 255;
        }
        page[27 + 16] = 1;
        page[27 + 17] = 10;
        page[27 + 18] = 0;
        page[27 + 19] = 0;
        file[PAGE_SIZE..PAGE_SIZE * 2].copy_from_slice(&page);

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut block).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        assert_eq!(r.next_packet(&mut buf), Err(TafError::NotAnOggPage));
        // A retry must not continue from a stale cursor with the remaining
        // lacing entries.
        assert_eq!(r.next_packet(&mut buf), Err(TafError::NotAnOggPage));
    }

    const CHAPTERS_FIXTURE: &[u8] = include_bytes!("../tests/data/chapters.taf");

    /// The fixture's chapter starts, as Ogg-stream page indices. Written as
    /// literals so the tests can disagree with the parser.
    const CHAPTERS_FIXTURE_PAGES: &[u32] = &[0, 5, 11];

    /// A saved position is a page the reader reported, so seeking to it must
    /// return to the same page.
    #[test]
    fn a_reader_returns_to_the_page_it_reported() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        r.seek_to_chapter(1).unwrap();
        let page = r.current_page();

        r.seek_to_chapter(0).unwrap();
        assert_ne!(r.current_page(), page, "the seek away has to move it");

        r.seek_to_page(page).unwrap();
        assert_eq!(r.current_page(), page);
    }

    /// Like `seek_to_chapter`: a page after the declared stream is padding,
    /// not audio.
    #[test]
    fn a_page_beyond_the_declared_stream_is_refused() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        assert_eq!(r.seek_to_page(u32::MAX), Err(TafError::PageOutOfRange));
    }

    /// File page 0 is the header, not audio.
    #[test]
    fn the_header_page_is_not_a_place_to_resume() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        assert_eq!(r.seek_to_page(0), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn the_fixtures_chapters_start_where_the_chapter_tests_assume() {
        let mut page = [0u8; PAGE_SIZE];
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        assert_eq!(r.header().chapter_pages.as_slice(), CHAPTERS_FIXTURE_PAGES);
    }

    #[test]
    fn a_freshly_opened_reader_is_in_the_first_chapter() {
        let mut page = [0u8; PAGE_SIZE];
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        assert_eq!(r.current_chapter(), 0);
    }

    #[test]
    fn seeking_reports_the_chapter_that_was_sought() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        r.seek_to_chapter(2).unwrap();
        assert_eq!(r.current_chapter(), 2);
        r.seek_to_chapter(1).unwrap();
        assert_eq!(r.current_chapter(), 1);
    }

    /// A story played straight through moves into later chapters without
    /// any seek, and the reported chapter must follow.
    #[test]
    fn reading_straight_through_reports_each_chapter_in_turn() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let mut seen: heapless::Vec<usize, 8> = heapless::Vec::new();
        seen.push(r.current_chapter()).unwrap();
        while r.next_packet(&mut buf).unwrap().is_some() {
            let now = r.current_chapter();
            if seen.last() != Some(&now) {
                seen.push(now).unwrap();
            }
        }
        assert_eq!(seen.as_slice(), &[0, 1, 2]);
    }

    /// A failed seek leaves the reader where it was, so the chapter must not
    /// change either.
    #[test]
    fn a_failed_seek_leaves_the_chapter_where_it_was() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        r.seek_to_chapter(1).unwrap();
        assert_eq!(r.seek_to_chapter(3), Err(TafError::PageOutOfRange));
        assert_eq!(r.current_chapter(), 1);
    }

    #[test]
    fn the_multi_chapter_fixture_has_three_chapters() {
        let mut page = [0u8; PAGE_SIZE];
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        assert_eq!(r.chapter_count(), 3);
    }

    #[test]
    fn multi_chapter_pages_are_strictly_increasing() {
        let mut page = [0u8; PAGE_SIZE];
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        let pages = r.header().chapter_pages.as_slice();
        for w in pages.windows(2) {
            assert!(
                w[0] < w[1],
                "chapter pages must be strictly increasing: {pages:?}"
            );
        }
    }

    #[test]
    fn seeking_to_each_chapter_succeeds_and_lands_on_different_audio() {
        let mut page0 = [0u8; PAGE_SIZE];
        let mut page1 = [0u8; PAGE_SIZE];
        let mut page2 = [0u8; PAGE_SIZE];
        // For each chapter, two freshly opened readers seek to it and must
        // return the same first packet, so the result does not depend on the
        // reader's earlier position. Then every chapter's first packet must
        // differ from the others, so a seek that does nothing would fail.
        let chapter_count = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page0)
            .unwrap()
            .chapter_count();

        let mut first_packets: heapless::Vec<([u8; MAX_PACKET], usize), 8> = heapless::Vec::new();

        for chapter in 0..chapter_count {
            let mut a =
                TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page1).unwrap();
            a.seek_to_chapter(chapter).unwrap();
            let mut buf_a = [0u8; MAX_PACKET];
            let len_a = a.next_packet(&mut buf_a).unwrap().expect("a packet");

            let mut b =
                TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page2).unwrap();
            b.seek_to_chapter(chapter).unwrap();
            let mut buf_b = [0u8; MAX_PACKET];
            let len_b = b.next_packet(&mut buf_b).unwrap().expect("a packet");

            assert_eq!(
                &buf_a[..len_a],
                &buf_b[..len_b],
                "two independent fresh readers seeking to chapter {chapter} must land \
                 on the same packet"
            );

            first_packets.push((buf_a, len_a)).unwrap();
        }

        for i in 0..first_packets.len() {
            for j in (i + 1)..first_packets.len() {
                let (buf_i, len_i) = &first_packets[i];
                let (buf_j, len_j) = &first_packets[j];
                assert_ne!(
                    &buf_i[..*len_i],
                    &buf_j[..*len_j],
                    "chapters {i} and {j}'s first packets must differ from each other"
                );
            }
        }
    }

    #[test]
    fn seeking_past_the_last_chapter_of_the_multi_chapter_fixture_is_an_error() {
        let mut page = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page).unwrap();
        let n = r.chapter_count();
        assert_eq!(r.seek_to_chapter(n), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn a_file_with_fewer_pages_than_the_header_declares_is_rejected_as_truncated() {
        let mut page = [0u8; PAGE_SIZE];
        // sine.taf declares data_length 57344 = 14 pages of Ogg stream (15
        // with the header). Cut to 10 whole pages, it would otherwise play as
        // a valid but shorter file.
        let truncated = &FIXTURE[..PAGE_SIZE * 10];
        assert!(matches!(
            TafReader::open(SlicePages::new(truncated).unwrap(), &mut page),
            Err(TafError::TruncatedFile)
        ));
    }

    #[test]
    fn both_real_fixtures_still_open_and_yield_their_existing_packet_counts() {
        let mut page0 = [0u8; PAGE_SIZE];
        let mut page1 = [0u8; PAGE_SIZE];
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page0).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let mut count = 0usize;
        while r.next_packet(&mut buf).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 85, "sine.taf's packet count must be unaffected");

        let mut r2 =
            TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap(), &mut page1).unwrap();
        let mut count2 = 0usize;
        while r2.next_packet(&mut buf).unwrap().is_some() {
            count2 += 1;
        }
        assert_eq!(count2, 97, "chapters.taf's packet count must be unaffected");
    }

    #[test]
    fn a_header_declaring_zero_data_length_is_rejected_not_opened() {
        let mut page = [0u8; PAGE_SIZE];
        // sine.taf's data_length varint is at file offsets 27..30, encoded as
        // [0x80, 0xC0, 0x03] (57344). Replace it with a 3-byte encoding of
        // zero ([0x80, 0x80, 0x00]) so no other field moves.
        //
        // With zero bytes of stream there is no page to open: even file
        // page 1 (OpusHead) is out of bounds.
        let mut file = [0u8; PAGE_SIZE * 15];
        file.copy_from_slice(FIXTURE);
        file[27..30].copy_from_slice(&[0x80, 0x80, 0x00]);

        assert!(
            TafReader::open(SlicePages::new(&file).unwrap(), &mut page).is_err(),
            "a header declaring zero data_length must not open"
        );
    }

    #[test]
    fn seeking_to_a_chapter_beyond_the_declared_stream_is_rejected_not_decoded() {
        let mut page = [0u8; PAGE_SIZE];
        // chapters.taf's data_length varint is at the same offsets (27..30),
        // encoded as [0x80, 0x80, 0x04] (65536, 16 pages). Replace it with
        // [0x80, 0xC0, 0x01] (24576, 6 pages), the same width, so no other
        // field moves.
        //
        // Chapter 2 (Ogg page 11, file page 12) is a valid Ogg page, but with
        // the smaller data_length it is no longer part of the stream. Seeking
        // there must fail.
        let mut file = [0u8; PAGE_SIZE * 17];
        file.copy_from_slice(CHAPTERS_FIXTURE);
        file[27..30].copy_from_slice(&[0x80, 0xC0, 0x01]);

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut page).unwrap();
        assert_eq!(r.seek_to_chapter(2), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn an_extra_all_zero_trailing_block_is_accepted_as_padding_and_ignored() {
        let mut page0 = [0u8; PAGE_SIZE];
        let mut page1 = [0u8; PAGE_SIZE];
        // Whole-page padding after the declared data is allowed: a real card
        // may have zero blocks left over from an older, longer recording.
        let mut padded = [0u8; PAGE_SIZE * 16];
        padded[..FIXTURE.len()].copy_from_slice(FIXTURE);
        // padded[FIXTURE.len()..] is left all zero by initialization.

        let mut original = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page0).unwrap();
        let mut padded_reader =
            TafReader::open(SlicePages::new(&padded).unwrap(), &mut page1).unwrap();

        let mut buf_a = [0u8; MAX_PACKET];
        let mut buf_b = [0u8; MAX_PACKET];
        loop {
            let a = original.next_packet(&mut buf_a).unwrap();
            let b = padded_reader.next_packet(&mut buf_b).unwrap();
            match (a, b) {
                (None, None) => break,
                (Some(na), Some(nb)) => assert_eq!(
                    &buf_a[..na],
                    &buf_b[..nb],
                    "padded file must yield exactly the same packets as the original"
                ),
                _ => {
                    panic!("packet stream length differs between the original and the padded file")
                }
            }
        }
    }

    #[test]
    fn a_dropped_fragment_does_not_hide_a_page_packed_in_behind_it() {
        let mut page = [0u8; PAGE_SIZE];
        // One block holding two Ogg pages. The first ends with a lacing entry
        // of 255 and no terminator, so its last packet is dropped. Its 255
        // bytes still take up space, and the second page starts after them.
        // A reader that did not skip them would miss the second page.
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[0x10, 0x80, 0x20]));

        let block = &mut file[PAGE_SIZE..];
        block[0..4].copy_from_slice(b"OggS");
        block[26] = 1;
        block[27] = 255; // unterminated: 255 bytes at 28..283, no more lacing

        const SECOND: usize = 283;
        block[SECOND..SECOND + 4].copy_from_slice(b"OggS");
        block[SECOND + 26] = 1;
        block[SECOND + 27] = 4;
        block[SECOND + 28..SECOND + 32].copy_from_slice(b"CCCC");

        let mut r = TafReader::open(SlicePages::new(&file).unwrap(), &mut page).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"CCCC");
    }

    /// A failing card: every page is in range, but one cannot be read.
    struct FailingPages<'a> {
        inner: SlicePages<'a>,
        unreadable: u32,
    }

    /// A source's own error type, unrelated to `TafError`, as on a real card.
    #[derive(Debug)]
    struct CardFault;

    impl PageSource for FailingPages<'_> {
        type Error = CardFault;

        fn read_page(&mut self, index: u32, buf: &mut [u8; PAGE_SIZE]) -> Result<(), CardFault> {
            if index == self.unreadable {
                return Err(CardFault);
            }
            self.inner.read_page(index, buf).map_err(|_| CardFault)
        }

        fn page_count(&self) -> u32 {
            self.inner.page_count()
        }
    }

    fn failing_at(page: u32) -> FailingPages<'static> {
        FailingPages {
            inner: SlicePages::new(FIXTURE).unwrap(),
            unreadable: page,
        }
    }

    #[test]
    fn a_page_that_cannot_be_read_is_an_io_error_not_a_range_error() {
        let mut page = [0u8; PAGE_SIZE];
        // Page 1 is within the file, so this is a storage error, not
        // `PageOutOfRange`.
        assert_eq!(
            TafReader::open(failing_at(1), &mut page).err(),
            Some(TafError::Io)
        );
    }

    #[test]
    fn a_header_that_cannot_be_read_is_an_io_error_not_a_malformed_header() {
        let mut page = [0u8; PAGE_SIZE];
        // The header may be fine; it just could not be read.
        assert_eq!(
            TafReader::open(failing_at(0), &mut page).err(),
            Some(TafError::Io)
        );
    }
}
