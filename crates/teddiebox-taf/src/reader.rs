//! Sequential and chapter-addressed reading over a `PageSource`.

use crate::{PacketCursor, PageSource, TafError, TonieHeader, PAGE_SIZE};

/// Buffer size that can hold any packet a TAF file can contain.
///
/// Derived from the container rather than from the codec. Every Ogg page in
/// a TAF file occupies at most one [`PAGE_SIZE`] block, and a packet lies
/// entirely within its own page's payload — so no packet can exceed
/// `PAGE_SIZE` minus the 27-byte page header and its lacing table,
/// whatever bitrate it was encoded at. A buffer of `PAGE_SIZE` is therefore
/// provably always enough.
///
/// The previous value, 1275, was RFC 6716's maximum *frame* size, and TAF's
/// 60 ms packets carry several frames each. It happened to hold for both
/// fixtures, whose packets reach 719 bytes, and its own documentation
/// flagged that this was not a guarantee. A real Toniebox file settled it:
/// of 67 107 packets, 179 exceed 1275 bytes and the largest is 4053, so
/// playback stopped 112 seconds in.
///
/// A packet larger than the caller's buffer still returns
/// `TafError::BufferTooSmall` rather than being truncated, and is left
/// unconsumed so a retry with a bigger buffer gets that same packet. That
/// remains reachable for a caller that supplies a smaller buffer than this.
pub const MAX_PACKET: usize = PAGE_SIZE;

pub struct TafReader<S: PageSource> {
    source: S,
    header: TonieHeader,
    /// Index of the currently-buffered *container block*, not of a real Ogg
    /// page: the nested-page scan in `next_packet` can walk several real Ogg
    /// pages within one buffered block without this changing.
    page_index: u32,
    page: [u8; PAGE_SIZE],
    /// Where the next packet starts within `page`. Held as a value rather
    /// than an iterator because it has to survive between calls, and this
    /// struct cannot borrow its own `page`.
    cursor: PacketCursor,
    /// File-page index of the last page the header's `data_length` declares
    /// as real Ogg stream. Trailing pages beyond this (e.g. benign padding)
    /// exist in `source` but must not be read as stream content: `next_packet`
    /// treats reaching past this index as end of stream, the same way it
    /// treats reaching the end of `source` itself.
    last_usable_page: u32,
    /// Serial of the Ogg stream this file carries, latched from the first
    /// page loaded. `None` only during `open`, before that page is read.
    ///
    /// Taken from the stream rather than from the header's `audio_id`
    /// deliberately. Both fixtures set the two equal, but that convention
    /// rests on `toniefile` alone — no commercial file has been examined —
    /// and enforcing it would reject every real `.taf` if it turns out not
    /// to hold. Self-consistency needs no such assumption and still catches
    /// the failure that matters: a block belonging to some other recording.
    stream_serial: Option<u32>,
}

impl<S: PageSource> TafReader<S> {
    pub fn open(mut source: S) -> Result<Self, TafError> {
        if source.page_count() < 2 {
            return Err(TafError::MalformedHeader);
        }
        let mut page = [0u8; PAGE_SIZE];
        source.read_page(0, &mut page).map_err(|_| TafError::Io)?;
        let header = TonieHeader::parse(&page)?;

        // The header declares how many bytes of Ogg stream follow it. Fewer
        // pages than that means the file was truncated -- decoding the prefix
        // would just stop the story early with nothing reporting it.
        let declared_pages = header.data_length.div_ceil(PAGE_SIZE as u32);
        let available_pages = source.page_count() - 1;
        if available_pages < declared_pages {
            return Err(TafError::TruncatedFile);
        }
        // A header declaring zero bytes of stream leaves no page for `open`
        // to position onto: file page 1, which must hold OpusHead, would be
        // beyond `last_usable_page` (0). This is a defect in the header's
        // own declared content, detectable without reference to how many
        // pages physically exist, so it is `MalformedHeader` rather than
        // `TruncatedFile` (which flags a mismatch between the declaration
        // and the physical file).
        if declared_pages == 0 {
            return Err(TafError::MalformedHeader);
        }

        let mut reader = Self {
            source,
            header,
            page_index: 0,
            page,
            cursor: PacketCursor::EMPTY,
            last_usable_page: declared_pages,
            stream_serial: None,
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

    /// Which chapter the reader is in, zero-based.
    ///
    /// Asked of the reader rather than remembered by whoever last sought,
    /// because a story played straight through crosses chapters with nobody
    /// seeking — and a caller counting from its own last seek would name the
    /// wrong chapter for the whole rest of the file.
    ///
    /// The answer is the last chapter start at or before the current page,
    /// using the same stream-relative-to-file conversion `seek_to_chapter`
    /// applies, so the two cannot disagree about where a chapter begins. A
    /// file whose chapter starts do not increase is not rejected here — the
    /// format does not promise they do — but the answer is only meaningful
    /// for one that does.
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
        // Read and validate into a scratch buffer before touching any of the
        // reader's own state. Committing `self.page` (or `self.page_index`
        // and the cursors) before `OggPage::parse` succeeds would leave the
        // buffer holding new, unvalidated bytes while the cursors still
        // describe the previous, valid page on a validation failure -- a
        // subsequent `next_packet` would then read the new bytes at offsets
        // computed for the old page and could silently return nonsense
        // instead of an error.
        let mut candidate = [0u8; PAGE_SIZE];
        // The range check above already rejected an index the source does not
        // have, so anything it refuses now is a failure of the medium rather
        // than of the request.
        self.source
            .read_page(index, &mut candidate)
            .map_err(|_| TafError::Io)?;
        let cursor = PacketCursor::at_page(&candidate, 0)?.ok_or(TafError::NotAnOggPage)?;
        self.check_stream(cursor.serial())?;

        self.page = candidate;
        self.page_index = index;
        self.cursor = cursor;
        Ok(())
    }

    /// Adopts the stream of the first page seen, and rejects any later page
    /// that does not belong to it.
    ///
    /// Called at every site that positions onto a page, including the scan
    /// for a page packed in behind another: a stale block is just as
    /// reachable there, and this is exactly the kind of obligation the two
    /// packet scanners drifted apart on before they were unified.
    fn check_stream(&mut self, serial: u32) -> Result<(), TafError> {
        match self.stream_serial {
            None => {
                self.stream_serial = Some(serial);
                Ok(())
            }
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
        // Bounded by `last_usable_page`, not just `source.page_count()`
        // (which `load_page` already checks): a chapter index can point at
        // a page that physically exists and parses as a real Ogg page --
        // benign trailing padding may itself be leftover `OggS` pages from
        // a previous, longer recording -- but which the header's
        // `data_length` does not declare as part of this stream. Without
        // this check, such a chapter would seek onto and decode that
        // undeclared page instead of failing.
        if file_page > self.last_usable_page {
            return Err(TafError::PageOutOfRange);
        }
        self.load_page(file_page)
    }

    /// Which container block the reader is on.
    ///
    /// This is what position memory's exact tier saves. A block index rather
    /// than an Ogg page index, because that is the unit the reader loads and
    /// therefore the only one it can be handed back.
    pub fn current_page(&self) -> u32 {
        self.page_index
    }

    /// Positions the reader at a block it reported earlier.
    ///
    /// Bounded exactly as [`seek_to_chapter`](Self::seek_to_chapter) is: a
    /// block past `last_usable_page` physically exists but is not declared as
    /// part of this stream, and file page 0 is the header rather than audio.
    ///
    /// A block that begins with the continuation of a packet started on the
    /// previous one costs the first few tens of milliseconds after the seek.
    /// That is the accepted price of resuming exactly rather than at a chapter
    /// boundary, and it is inaudible against a story.
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
            // Both failures below must leave the reader exactly as it was:
            // `BufferTooSmall` promises a retry with a bigger buffer gets
            // this same packet, and a corrupt page must not let a retry
            // resume half-advanced and fabricate a packet out of whatever
            // lacing entries happen to follow. Rewinding to a saved position
            // states that directly, where an un-advanced local cursor only
            // implied it.
            let saved = self.cursor;
            match self.cursor.next(&self.page) {
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

            // This Ogg page is exhausted. A single PAGE_SIZE-byte container
            // block can hold more than one real Ogg page back to back:
            // `toniefile` packs the tiny OpusHead and OpusTags pages
            // together with the start of the audio stream rather than
            // leaving the rest of the block empty. Look for another page
            // behind this one before reading a new block from the source.
            //
            // Without this, the reader silently drops every packet packed
            // behind the first page in a block instead of erroring: it
            // looks like a clean end-of-page and just advances past them.
            if let Some(nested) = PacketCursor::at_page(&self.page, self.cursor.payload_start())? {
                self.check_stream(nested.serial())?;
                self.cursor = nested;
                continue;
            }

            // No further real Ogg page in this block: advance to the next
            // container block, or report end of stream. Bounded by
            // `last_usable_page`, not `source.page_count()`: a page-aligned
            // truncated file is rejected in `open`, but a file with benign
            // trailing padding past the declared data must still stop here
            // rather than reading that padding as stream content.
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
    // Renamed from `seeking_to_chapter_zero_lands_on_audio_not_the_header`:
    // that name claimed to distinguish landing on real audio from landing on
    // a header, but the assertion below only ever checked that the returned
    // bytes aren't a raw, unparsed page (i.e. that the packet parser skipped
    // past the page header and lacing table). It says nothing about audio
    // versus Opus-header packets -- the first packet chapter 0 actually
    // yields is `OpusHead`, not audio. A misleading name here already misled
    // a reviewer about unrelated code; this name now matches what the test
    // checks instead of overstating it.
    fn seeking_to_chapter_zero_returns_packet_payload_not_a_raw_unparsed_page() {
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        r.seek_to_chapter(0).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let n = r.next_packet(&mut buf).unwrap().expect("a packet");
        // The off-by-one is already caught above: without the `+1`,
        // `seek_to_chapter(0)` would load the TAF header page, `OggPage::parse`
        // would reject it, and the `.unwrap()` on the line above would panic.
        // This assertion guards a different regression: that the packet
        // returned is real payload content skipped past the page header and
        // lacing table, not a raw, unparsed page starting with the page's
        // own capture pattern.
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

    #[test]
    fn reads_a_packet_as_large_as_a_page_can_hold() {
        // 4053 bytes is the largest packet measured in a real Toniebox file,
        // and it is more than three times the old buffer. Nothing in either
        // fixture comes close — their packets peak at 719 — so only a
        // synthetic page can pin this.
        //
        // data_length (field 2) = 4096: one Ogg page of stream.
        let packet_len = 4053usize;
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[0x10, 0x80, 0x20]));
        file[PAGE_SIZE..].copy_from_slice(&ogg_page(&[&[0xAB; 4053]]));

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(n, packet_len);
        assert!(buf[..n].iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn a_packet_too_large_for_the_buffer_can_be_retried_with_a_bigger_one() {
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();

        // "OpusHead" is 19 bytes; 4 bytes surely doesn't fit.
        let mut too_small = [0u8; 4];
        assert_eq!(r.next_packet(&mut too_small), Err(TafError::BufferTooSmall));

        // The failed attempt must not have consumed the packet: retrying
        // with a big-enough buffer gets the very same packet, not the one
        // after it.
        let mut big_enough = [0u8; MAX_PACKET];
        let n = r.next_packet(&mut big_enough).unwrap().unwrap();
        assert_eq!(&big_enough[..n.min(8)], b"OpusHead");
    }

    /// Builds a minimal TAF header page carrying the given protobuf field
    /// bytes, mirroring `header.rs`'s own test helper (private to that
    /// module, so duplicated here rather than reused).
    fn header_page(fields: &[u8]) -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(&(fields.len() as u32).to_be_bytes());
        page[4..4 + fields.len()].copy_from_slice(fields);
        page
    }

    /// Assembles a minimal valid Ogg page carrying the given packets,
    /// mirroring `page.rs`'s own test helper (private to that module, so
    /// duplicated here rather than reused).
    fn ogg_page(packets: &[&[u8]]) -> [u8; PAGE_SIZE] {
        ogg_page_with_serial(0, packets)
    }

    /// As `ogg_page`, but stamped with a stream serial. Only the tests that
    /// care about stream identity need to say which stream a page belongs to.
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
    fn a_block_belonging_to_another_stream_is_rejected_rather_than_decoded() {
        // The torn-write failure mode: page 2 is a structurally perfect Ogg
        // page sitting at a perfectly valid offset, left over from a
        // previous, longer recording. Nothing about its shape gives it away
        // — only that it belongs to a different stream. Decoding it is how a
        // child ends up hearing the end of the previous story.
        //
        // data_length (field 2) = 8192, declaring both pages as stream.
        let mut file = [0u8; PAGE_SIZE * 3];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[0x10, 0x80, 0x40]));
        file[PAGE_SIZE..PAGE_SIZE * 2].copy_from_slice(&ogg_page_with_serial(0xAAAA, &[b"AAAA"]));
        file[PAGE_SIZE * 2..].copy_from_slice(&ogg_page_with_serial(0xBBBB, &[b"BBBB"]));

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"AAAA");
        assert_eq!(r.next_packet(&mut buf), Err(TafError::WrongStream));
    }

    #[test]
    fn a_page_packed_behind_another_is_checked_against_the_stream_too() {
        // The same stale page, but packed in behind a good one inside a
        // single block rather than starting one. That is a second
        // positioning site, and it is where the two packet scanners drifted
        // apart before they were unified — so it gets its own test rather
        // than trusting that one check covers both.
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[0x10, 0x80, 0x20]));

        let block = &mut file[PAGE_SIZE..];
        block[..PAGE_SIZE].copy_from_slice(&ogg_page_with_serial(0xAAAA, &[b"AAAA"]));

        // A second page immediately behind the first one's payload.
        const SECOND: usize = 27 + 1 + 4;
        let foreign = ogg_page_with_serial(0xBBBB, &[b"BBBB"]);
        block[SECOND..SECOND + 32].copy_from_slice(&foreign[..32]);

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"AAAA");
        assert_eq!(r.next_packet(&mut buf), Err(TafError::WrongStream));
    }

    #[test]
    fn seeking_onto_a_block_from_another_stream_is_rejected() {
        // Trailing padding may itself be leftover `OggS` pages from a longer
        // previous recording, and a chapter index inside the declared range
        // can point straight at one. `last_usable_page` does not help here:
        // the page is declared stream, it just isn't this stream's.
        //
        // data_length = 8192; chapter_pages (field 4, packed) = [0, 1].
        let mut file = [0u8; PAGE_SIZE * 3];
        file[0..PAGE_SIZE]
            .copy_from_slice(&header_page(&[0x10, 0x80, 0x40, 0x22, 0x02, 0x00, 0x01]));
        file[PAGE_SIZE..PAGE_SIZE * 2].copy_from_slice(&ogg_page_with_serial(0xAAAA, &[b"AAAA"]));
        file[PAGE_SIZE * 2..].copy_from_slice(&ogg_page_with_serial(0xBBBB, &[b"BBBB"]));

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();

        assert_eq!(r.seek_to_chapter(1), Err(TafError::WrongStream));
    }

    #[test]
    fn a_failed_seek_leaves_the_reader_positioned_where_it_was() {
        // Chapter 0 at ogg page 0 (file page 1, holding two packets);
        // chapter 1 at ogg page 1 (file page 2, deliberately not a real Ogg
        // page at all).
        //
        // data_length (field 2) = 8192 = two pages, declaring both page 1
        // and page 2 as usable: chapter 1 must fail because page 2 isn't a
        // real Ogg page, not merely because it's out of the declared range
        // (that distinct failure mode is covered by the "beyond the
        // declared stream" tests above).
        let mut file = [0u8; PAGE_SIZE * 3];
        file[0..PAGE_SIZE]
            .copy_from_slice(&header_page(&[0x10, 0x80, 0x40, 0x22, 0x02, 0x00, 0x01]));
        file[PAGE_SIZE..PAGE_SIZE * 2].copy_from_slice(&ogg_page(&[b"AAAA", b"BBBB"]));
        // file[PAGE_SIZE * 2..] is left all zero: not "OggS", not a page.

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"AAAA");

        assert_eq!(
            r.seek_to_chapter(1),
            Err(TafError::NotAnOggPage),
            "chapter 1 points at a page that isn't a real Ogg page"
        );

        // The failed seek must not have clobbered the buffer or the
        // cursors: the reader should still be exactly where the last
        // successful read left it, not reading garbage assembled from a
        // buffer that no longer matches its own position.
        let n2 = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n2], b"BBBB");
    }

    #[test]
    fn an_oversized_packet_does_not_leave_stale_state_for_a_retry() {
        // data_length (field 2) = 4096 = one page, declaring the single Ogg
        // page below as usable.
        let mut file = [0u8; PAGE_SIZE * 2];
        file[0..PAGE_SIZE].copy_from_slice(&header_page(&[0x10, 0x80, 0x20, 0x18, 0x07]));

        // 20 segments: the first 17 (sixteen 255s plus a terminating 1)
        // declare a 4081-byte packet starting at payload offset 47, which
        // overruns the 4096-byte page. Three lacing entries remain after it
        // (mirrors page.rs's equivalent test for `OggPage::packets`).
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

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        assert_eq!(r.next_packet(&mut buf), Err(TafError::NotAnOggPage));
        // A retry must not resume from a stale cursor and fabricate a
        // packet out of the lacing entries that followed the oversized one.
        assert_eq!(r.next_packet(&mut buf), Err(TafError::NotAnOggPage));
    }

    const CHAPTERS_FIXTURE: &[u8] = include_bytes!("../tests/data/chapters.taf");

    /// The fixture's chapter starts, as Ogg-stream page indices. Stated
    /// literally so these tests can disagree with the parser rather than
    /// asking it what to expect.
    const CHAPTERS_FIXTURE_PAGES: &[u32] = &[0, 5, 11];

    /// Position memory's exact tier resumes at a page the reader itself
    /// reported, so what it hands out must be what it takes back.
    #[test]
    fn a_reader_returns_to_the_page_it_reported() {
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        r.seek_to_chapter(1).unwrap();
        let page = r.current_page();

        r.seek_to_chapter(0).unwrap();
        assert_ne!(r.current_page(), page, "the seek away has to move it");

        r.seek_to_page(page).unwrap();
        assert_eq!(r.current_page(), page);
    }

    /// The same bound `seek_to_chapter` applies: a page that physically exists
    /// but sits beyond what the header declares is padding, not audio, and
    /// decoding it would produce noise from a previous, longer recording.
    #[test]
    fn a_page_beyond_the_declared_stream_is_refused() {
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        assert_eq!(r.seek_to_page(u32::MAX), Err(TafError::PageOutOfRange));
    }

    /// File page 0 is the header, never audio — the `+1` in `seek_to_chapter`
    /// exists for exactly this reason.
    #[test]
    fn the_header_page_is_not_a_place_to_resume() {
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        assert_eq!(r.seek_to_page(0), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn the_fixtures_chapters_start_where_the_chapter_tests_assume() {
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        assert_eq!(r.header().chapter_pages.as_slice(), CHAPTERS_FIXTURE_PAGES);
    }

    #[test]
    fn a_freshly_opened_reader_is_in_the_first_chapter() {
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        assert_eq!(r.current_chapter(), 0);
    }

    #[test]
    fn seeking_reports_the_chapter_that_was_sought() {
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        r.seek_to_chapter(2).unwrap();
        assert_eq!(r.current_chapter(), 2);
        r.seek_to_chapter(1).unwrap();
        assert_eq!(r.current_chapter(), 1);
    }

    /// The reason this is asked of the reader rather than tracked by whoever
    /// last seeked: a story played straight through crosses chapters without
    /// anybody seeking, and "skip to the next one" is wrong by a whole
    /// chapter if it counts from the last seek instead of from where the
    /// stream actually is.
    #[test]
    fn reading_straight_through_reports_each_chapter_in_turn() {
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
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

    /// `seek_to_chapter` promises a failed seek leaves the reader exactly
    /// where it was. The chapter it reports has to keep that promise too, or
    /// a skip past the end would silently renumber the story.
    #[test]
    fn a_failed_seek_leaves_the_chapter_where_it_was() {
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        r.seek_to_chapter(1).unwrap();
        assert_eq!(r.seek_to_chapter(3), Err(TafError::PageOutOfRange));
        assert_eq!(r.current_chapter(), 1);
    }

    #[test]
    fn the_multi_chapter_fixture_has_three_chapters() {
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        assert_eq!(r.chapter_count(), 3);
    }

    #[test]
    fn multi_chapter_pages_are_strictly_increasing() {
        let r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
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
        // Regression note: the previous version of this test reused one
        // reader across iterations and compared every chapter only against
        // a chapter-0 baseline captured up front. A `seek_to_chapter` that
        // merely bounds-checked its argument without actually repositioning
        // would still pass that version, because the shared reader would
        // already be correctly positioned from whichever seek last actually
        // ran. This matters beyond the test itself: chapter seeking is what
        // track-skip compiles down to.
        //
        // Strengthened: for each chapter, seek two *independent*, freshly
        // opened readers to it and require their first packets to agree
        // (proving the seek is reproducible, not an accident of the
        // reader's prior position), then require all chapters' first
        // packets to be pairwise different from each other. A no-op seek
        // fails this: every chapter would land on the same page the reader
        // opens onto by default, making all "first packets" identical.
        let chapter_count = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap())
            .unwrap()
            .chapter_count();

        let mut first_packets: heapless::Vec<([u8; MAX_PACKET], usize), 8> = heapless::Vec::new();

        for chapter in 0..chapter_count {
            let mut a = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
            a.seek_to_chapter(chapter).unwrap();
            let mut buf_a = [0u8; MAX_PACKET];
            let len_a = a.next_packet(&mut buf_a).unwrap().expect("a packet");

            let mut b = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
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
        let mut r = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        let n = r.chapter_count();
        assert_eq!(r.seek_to_chapter(n), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn a_file_with_fewer_pages_than_the_header_declares_is_rejected_as_truncated() {
        // sine.taf declares data_length 57344 = 14 pages of Ogg stream
        // (page_count 15, header included). Slicing to just the first 10
        // blocks is page-aligned -- so without checking `data_length` this
        // would otherwise open and decode to a valid, silently shorter file.
        let truncated = &FIXTURE[..PAGE_SIZE * 10];
        assert!(matches!(
            TafReader::open(SlicePages::new(truncated).unwrap()),
            Err(TafError::TruncatedFile)
        ));
    }

    #[test]
    fn both_real_fixtures_still_open_and_yield_their_existing_packet_counts() {
        let mut r = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let mut count = 0usize;
        while r.next_packet(&mut buf).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 85, "sine.taf's packet count must be unaffected");

        let mut r2 = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
        let mut count2 = 0usize;
        while r2.next_packet(&mut buf).unwrap().is_some() {
            count2 += 1;
        }
        assert_eq!(count2, 97, "chapters.taf's packet count must be unaffected");
    }

    #[test]
    fn a_header_declaring_zero_data_length_is_rejected_not_opened() {
        // sine.taf's data_length varint lives at file offsets 27..30, encoded
        // canonically as [0x80, 0xC0, 0x03] (57344). Overwrite it with a
        // non-canonical 3-byte encoding of zero ([0x80, 0x80, 0x00]) so the
        // field keeps the same byte width and every following field's offset
        // is undisturbed -- only the decoded value changes.
        //
        // A header declaring zero bytes of Ogg stream has no page for
        // `open` to position onto: `last_usable_page` would be 0, meaning
        // even file page 1 (which must hold OpusHead) is out of bounds.
        // Before this was enforced, `open` positioned onto page 1
        // unconditionally and happily decoded it as if it were declared
        // stream content.
        let mut file = [0u8; PAGE_SIZE * 15];
        file.copy_from_slice(FIXTURE);
        file[27..30].copy_from_slice(&[0x80, 0x80, 0x00]);

        assert!(
            TafReader::open(SlicePages::new(&file).unwrap()).is_err(),
            "a header declaring zero data_length must not open"
        );
    }

    #[test]
    fn seeking_to_a_chapter_beyond_the_declared_stream_is_rejected_not_decoded() {
        // chapters.taf's data_length varint lives at the same file offsets
        // (27..30), encoded canonically as [0x80, 0x80, 0x04] (65536, 16
        // pages). Overwrite it with [0x80, 0xC0, 0x01] (24576, 6 pages): a
        // same-width, still-canonical 3-byte varint, so nothing else in the
        // header shifts.
        //
        // Chapter 2 (ogg page 11, file page 12) is a real, valid Ogg page
        // physically present in the file -- it is genuine leftover stream
        // data, not zeroed padding -- but with the shrunk data_length it is
        // no longer part of the declared stream. Seeking there must fail,
        // not silently decode that undeclared page as if it were this
        // file's audio.
        let mut file = [0u8; PAGE_SIZE * 17];
        file.copy_from_slice(CHAPTERS_FIXTURE);
        file[27..30].copy_from_slice(&[0x80, 0xC0, 0x01]);

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();
        assert_eq!(r.seek_to_chapter(2), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn an_extra_all_zero_trailing_block_is_accepted_as_padding_and_ignored() {
        // Page-aligned padding after the declared data must stay acceptable
        // (unlike an actually truncated file): a real device may leave
        // trailing zero blocks from a previous, longer recording.
        let mut padded = [0u8; PAGE_SIZE * 16];
        padded[..FIXTURE.len()].copy_from_slice(FIXTURE);
        // padded[FIXTURE.len()..] is left all zero by initialization.

        let mut original = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut padded_reader = TafReader::open(SlicePages::new(&padded).unwrap()).unwrap();

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
        // One container block holding two real Ogg pages. The first ends
        // with a lacing entry of 255 and no terminator, so its last packet
        // continues onto a page that does not exist and gets dropped. Those
        // 255 bytes are still declared, still occupy the payload, and the
        // second page starts after them.
        //
        // A reader that drops the fragment without stepping over its bytes
        // looks for the next page at the fragment's own start, does not find
        // one, and silently loses every packet behind it.
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

        let mut r = TafReader::open(SlicePages::new(&file).unwrap()).unwrap();
        let mut buf = [0u8; MAX_PACKET];

        let n = r.next_packet(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"CCCC");
    }

    /// What a card that is present, correctly sized, and failing looks like:
    /// every page is in range, but one of them will not read.
    struct FailingPages<'a> {
        inner: SlicePages<'a>,
        unreadable: u32,
    }

    /// A source's own error type, deliberately nothing to do with `TafError`
    /// — a real one reports CRC failures and timeouts, which this layer has
    /// no vocabulary for.
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
        // Page 1 is well within the file. Reporting this as `PageOutOfRange`
        // would send anyone debugging it looking for a seek bug in a reader
        // that is behaving perfectly.
        assert_eq!(TafReader::open(failing_at(1)).err(), Some(TafError::Io));
    }

    #[test]
    fn a_header_that_cannot_be_read_is_an_io_error_not_a_malformed_header() {
        // The header may be perfectly well-formed; nobody managed to look at
        // it. Blaming the file's contents for a failure of the medium sends
        // debugging after the wrong artefact entirely.
        assert_eq!(TafReader::open(failing_at(0)).err(), Some(TafError::Io));
    }
}
