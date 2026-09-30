#![no_std]

mod opus;
pub use opus::{LibOpus, OpusState, OPUS_STATE_BYTES};

use teddiebox_taf::{PageSource, TafError, TafReader, TonieHeader, MAX_PACKET, PAGE_SIZE};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Opus frames are at most 120 ms; at 48 kHz stereo that is the largest PCM
/// buffer a single packet can produce.
pub const MAX_FRAME_SAMPLES: usize = 5760 * CHANNELS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioError {
    Container(TafError),
    Decode,
    BufferTooSmall,
}

impl From<TafError> for AudioError {
    fn from(e: TafError) -> Self {
        AudioError::Container(e)
    }
}

impl core::fmt::Display for AudioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AudioError::Container(e) => write!(f, "container error: {e}"),
            AudioError::Decode => f.write_str("Opus decode failed"),
            AudioError::BufferTooSmall => f.write_str("PCM buffer too small for one frame"),
        }
    }
}

impl core::error::Error for AudioError {}

/// Decodes one Opus packet into interleaved 16-bit PCM.
pub trait OpusDecode {
    /// Returns the number of samples written (frames × channels).
    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError>;
}

/// True for the two Opus stream-header packets, which carry ASCII magic that
/// no audio frame realistically begins with.
fn is_opus_header(packet: &[u8]) -> bool {
    packet.starts_with(b"OpusHead") || packet.starts_with(b"OpusTags")
}

/// Where a skip forward left the story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Now playing this chapter, zero-based.
    To(usize),
    /// There was no next chapter, so the story is over.
    PastTheEnd,
}

/// The two blocks a [`TafDecoder`] works in, each [`PAGE_SIZE`] bytes: the file block being read
/// and the packet being decoded.
///
/// Lent to the decoder rather than held by it, so the decoder itself is
/// small. On the box a decoder lives inside nested async functions, and each
/// level copies its value between stack frames; blocks held inline were
/// copied at every level and overflowed the stack. The caller keeps these in
/// one place for as long as it decodes.
pub struct TafBuffers {
    page: [u8; PAGE_SIZE],
    packet: [u8; MAX_PACKET],
}

impl TafBuffers {
    pub const fn new() -> Self {
        Self {
            page: [0u8; PAGE_SIZE],
            packet: [0u8; MAX_PACKET],
        }
    }
}

impl Default for TafBuffers {
    fn default() -> Self {
        Self::new()
    }
}

pub struct TafDecoder<'b, S: PageSource, D: OpusDecode> {
    reader: TafReader<'b, S>,
    decoder: D,
    /// Big enough for any packet a TAF file can contain; see [`MAX_PACKET`].
    packet: &'b mut [u8; MAX_PACKET],
}

impl<'b, S: PageSource, D: OpusDecode> TafDecoder<'b, S, D> {
    /// Opens a source and takes ownership of the reader, working in
    /// `buffers`.
    ///
    /// Does **not** accept a ready-made `TafReader`, so no caller can move the
    /// reader without the decoder knowing.
    pub fn open(source: S, decoder: D, buffers: &'b mut TafBuffers) -> Result<Self, AudioError> {
        Ok(Self {
            reader: TafReader::open(source, &mut buffers.page)?,
            decoder,
            packet: &mut buffers.packet,
        })
    }

    pub fn header(&self) -> &TonieHeader {
        self.reader.header()
    }

    pub fn chapter_count(&self) -> usize {
        self.reader.chapter_count()
    }

    /// Which chapter is playing, zero-based.
    ///
    /// Asked of the reader, because a story played straight through moves
    /// into later chapters without any seek.
    pub fn chapter(&self) -> usize {
        self.reader.current_chapter()
    }

    /// Seeks to a chapter and resumes decoding from there.
    ///
    /// A failed seek leaves the reader where it was, as
    /// `TafReader::seek_to_chapter` does.
    pub fn seek_to_chapter(&mut self, n: usize) -> Result<(), AudioError> {
        self.reader.seek_to_chapter(n)?;
        Ok(())
    }

    /// Which 4096-byte block the reader is on. This is the position saved
    /// when a figure is lifted.
    pub fn page(&self) -> u32 {
        self.reader.current_page()
    }

    /// Resumes at a page reported earlier by [`page`](Self::page).
    ///
    /// A failed seek leaves the reader where it was, like
    /// [`seek_to_chapter`](Self::seek_to_chapter), so a saved page from
    /// another story does no harm.
    pub fn seek_to_page(&mut self, page: u32) -> Result<(), AudioError> {
        self.reader.seek_to_page(page)?;
        Ok(())
    }

    /// Skips forward to the start of the following chapter.
    ///
    /// Returns [`Skip::PastTheEnd`] from the last chapter and does not move:
    /// skipping past the last chapter means the story is finished, and the
    /// caller ends it.
    pub fn next_chapter(&mut self) -> Result<Skip, AudioError> {
        let next = self.chapter() + 1;
        if next >= self.chapter_count() {
            return Ok(Skip::PastTheEnd);
        }
        self.seek_to_chapter(next)?;
        Ok(Skip::To(next))
    }

    /// Skips back to the start of the previous chapter, or restarts the
    /// current one if there is none.
    ///
    /// Restarts rather than doing nothing, because a control that does
    /// nothing looks broken.
    pub fn previous_chapter(&mut self) -> Result<usize, AudioError> {
        let previous = self.chapter().saturating_sub(1);
        self.seek_to_chapter(previous)?;
        Ok(previous)
    }

    /// Decodes the next audio packet. `Ok(None)` at end of stream.
    ///
    /// Errors behave differently on retry. After an [`AudioError::Container`]
    /// the packet is not consumed, so a retry tries it again. An
    /// [`AudioError::Decode`] happens after the packet was consumed, so a
    /// retry continues with the next packet. Treat a decode error as one lost
    /// frame.
    pub fn next_frame(&mut self, pcm: &mut [i16]) -> Result<Option<usize>, AudioError> {
        if pcm.len() < MAX_FRAME_SAMPLES {
            return Err(AudioError::BufferTooSmall);
        }
        // OpusHead and OpusTags must not reach libopus. They are recognised
        // by their magic bytes, not their position, because after seeking to
        // chapter 0 the reader starts *at* OpusHead.
        loop {
            match self.reader.next_packet(self.packet) {
                Ok(None) => return Ok(None),
                Ok(Some(len)) if is_opus_header(&self.packet[..len]) => continue,
                Ok(Some(len)) => {
                    let n = self.decoder.decode(&self.packet[..len], pcm)?;
                    return Ok(Some(n));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teddiebox_taf::SlicePages;

    const FIXTURE: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/sine.taf");
    const CHAPTERS: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/chapters.taf");

    /// Opens `taf` with `decoder` in `buffers`.
    fn open<'b, D: OpusDecode>(
        taf: &'static [u8],
        decoder: D,
        buffers: &'b mut TafBuffers,
    ) -> TafDecoder<'b, SlicePages<'static>, D> {
        TafDecoder::open(SlicePages::new(taf).unwrap(), decoder, buffers).unwrap()
    }

    /// Records what it was asked to decode and emits silence, so the container
    /// plumbing can be tested without libopus.
    struct StubDecoder {
        calls: usize,
        saw_opus_header: bool,
    }

    impl OpusDecode for StubDecoder {
        fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError> {
            self.calls += 1;
            if packet.starts_with(b"OpusHead") || packet.starts_with(b"OpusTags") {
                self.saw_opus_header = true;
            }
            let n = 960 * CHANNELS;
            pcm[..n].fill(0);
            Ok(n)
        }
    }

    fn stub() -> StubDecoder {
        StubDecoder {
            calls: 0,
            saw_opus_header: false,
        }
    }

    #[test]
    fn the_opus_header_packets_are_never_sent_to_the_decoder() {
        // Given
        let mut buffers = TafBuffers::new();
        let mut dec = open(FIXTURE, stub(), &mut buffers);
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        // When: the whole stream, not just the first packet
        while dec.next_frame(&mut pcm).unwrap().is_some() {}

        // Then
        assert!(!dec.decoder.saw_opus_header);
    }

    #[test]
    fn decodes_every_audio_packet_then_reports_end_of_stream() {
        // Given: 85 packets, OpusHead, OpusTags and 83 audio frames. `toniefile`
        // encodes 60 ms Opus frames (2880 samples/channel): 83 x 2880 = 239040
        // samples = 4.98 s.
        let mut buffers = TafBuffers::new();
        let mut dec = open(FIXTURE, stub(), &mut buffers);
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        // When
        let mut frames = 0;
        while dec.next_frame(&mut pcm).unwrap().is_some() {
            frames += 1;
        }
        let after_the_end = dec.next_frame(&mut pcm).unwrap();

        // Then
        assert_eq!(frames, 83, "expected 83 audio frames, got {frames}");
        assert_eq!(after_the_end, None);
    }

    #[test]
    fn rejects_a_pcm_buffer_that_is_too_small() {
        // Given
        let mut buffers = TafBuffers::new();
        let mut dec = open(FIXTURE, stub(), &mut buffers);
        let mut pcm = [0i16; 8];

        // When
        let frame = dec.next_frame(&mut pcm);

        // Then
        assert_eq!(frame, Err(AudioError::BufferTooSmall));
    }

    /// A chapter skip needs to know the current chapter, so the decoder
    /// reports it.
    #[test]
    fn the_decoder_reports_the_chapter_it_is_playing() {
        // Given
        let mut buffers = TafBuffers::new();
        let mut dec = open(CHAPTERS, stub(), &mut buffers);
        assert_eq!(dec.chapter(), 0);

        // When
        dec.seek_to_chapter(2).unwrap();

        // Then
        assert_eq!(dec.chapter(), 2);
    }

    /// The firmware only holds a decoder, not a reader, so the decoder must
    /// pass page positions through.
    #[test]
    fn the_decoder_returns_to_the_page_it_reported() {
        // Given: the page chapter 2 starts on, then a seek away from it
        let mut buffers = TafBuffers::new();
        let mut dec = open(CHAPTERS, stub(), &mut buffers);
        dec.seek_to_chapter(2).unwrap();
        let page = dec.page();
        dec.seek_to_chapter(0).unwrap();
        assert_ne!(dec.page(), page, "the seek away has to move it");

        // When
        dec.seek_to_page(page).unwrap();

        // Then
        assert_eq!(dec.page(), page);
        assert_eq!(dec.chapter(), 2, "and it is back in that chapter");
    }

    /// Records the last packet it was asked to decode, so a test can see
    /// where in the file the decoder is.
    struct LastPacket {
        buf: [u8; MAX_PACKET],
        len: usize,
    }
    impl OpusDecode for LastPacket {
        fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError> {
            self.buf[..packet.len()].copy_from_slice(packet);
            self.len = packet.len();
            let n = 960 * CHANNELS;
            pcm[..n].fill(0);
            Ok(n)
        }
    }

    fn chapters_decoder(
        buffers: &mut TafBuffers,
    ) -> TafDecoder<'_, SlicePages<'static>, LastPacket> {
        open(
            CHAPTERS,
            LastPacket {
                buf: [0u8; MAX_PACKET],
                len: 0,
            },
            buffers,
        )
    }

    #[test]
    fn the_next_chapter_is_the_one_after_the_chapter_playing() {
        // Given
        let mut buffers = TafBuffers::new();
        let mut dec = chapters_decoder(&mut buffers);

        // When
        let skips = [(); 2].map(|_| (dec.next_chapter(), dec.chapter()));

        // Then
        assert_eq!(skips, [(Ok(Skip::To(1)), 1), (Ok(Skip::To(2)), 2)]);
    }

    /// Skipping forward from the last chapter ends the story, instead of
    /// playing the last chapter again.
    #[test]
    fn there_is_no_chapter_after_the_last_one() {
        // Given
        let mut buffers = TafBuffers::new();
        let mut dec = chapters_decoder(&mut buffers);
        dec.seek_to_chapter(2).unwrap();

        // When
        let skip = dec.next_chapter();

        // Then
        assert_eq!(skip, Ok(Skip::PastTheEnd));
        assert_eq!(dec.chapter(), 2, "a refused skip must not move the story");
    }

    #[test]
    fn the_previous_chapter_is_the_one_before_the_chapter_playing() {
        // Given
        let mut buffers = TafBuffers::new();
        let mut dec = chapters_decoder(&mut buffers);
        dec.seek_to_chapter(2).unwrap();

        // When
        let back = dec.previous_chapter();

        // Then
        assert_eq!(back, Ok(1));
        assert_eq!(dec.chapter(), 1);
    }

    /// Going back from the first chapter restarts it, rather than doing
    /// nothing.
    #[test]
    fn going_back_from_the_first_chapter_starts_it_again() {
        // Given: the first packet noted, then two more decoded
        let mut buffers = TafBuffers::new();
        let mut dec = chapters_decoder(&mut buffers);
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];
        dec.next_frame(&mut pcm).unwrap();
        let mut first = [0u8; MAX_PACKET];
        let first_len = dec.decoder.len;
        first[..first_len].copy_from_slice(&dec.decoder.buf[..first_len]);
        dec.next_frame(&mut pcm).unwrap();
        dec.next_frame(&mut pcm).unwrap();
        assert_ne!(
            &dec.decoder.buf[..dec.decoder.len],
            &first[..first_len],
            "the fixture must have moved on, or this proves nothing"
        );

        // When
        let back = dec.previous_chapter();
        dec.next_frame(&mut pcm).unwrap();

        // Then
        assert_eq!(back, Ok(0));
        assert_eq!(&dec.decoder.buf[..dec.decoder.len], &first[..first_len]);
    }

    #[test]
    fn seeking_to_a_chapter_decodes_its_first_packet_not_a_later_one() {
        // Given: chapter 1's first packet, read without `TafDecoder`
        let mut buffers = TafBuffers::new();
        let mut page = [0u8; PAGE_SIZE];
        let mut direct = TafReader::open(SlicePages::new(CHAPTERS).unwrap(), &mut page).unwrap();
        direct.seek_to_chapter(1).unwrap();
        let mut expected = [0u8; MAX_PACKET];
        let expected_len = direct.next_packet(&mut expected).unwrap().unwrap();

        /// Records the first packet it is asked to decode.
        struct CapturingDecoder {
            first_packet: Option<([u8; MAX_PACKET], usize)>,
        }
        impl OpusDecode for CapturingDecoder {
            fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError> {
                if self.first_packet.is_none() {
                    let mut buf = [0u8; MAX_PACKET];
                    buf[..packet.len()].copy_from_slice(packet);
                    self.first_packet = Some((buf, packet.len()));
                }
                let n = 960 * CHANNELS;
                pcm[..n].fill(0);
                Ok(n)
            }
        }
        let mut dec = open(
            CHAPTERS,
            CapturingDecoder { first_packet: None },
            &mut buffers,
        );

        // When
        dec.seek_to_chapter(1).unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];
        dec.next_frame(&mut pcm).unwrap();

        // Then
        let (buf, len) = dec.decoder.first_packet.expect("decode was called");
        assert_eq!(
            &buf[..len],
            &expected[..expected_len],
            "expected chapter 1's first packet, got a different one \
             (the positional header skip re-triggered after a seek)"
        );
    }

    #[test]
    fn a_decode_error_consumes_the_packet_so_the_next_call_resumes_after_it() {
        // Given: the second real audio packet, read without `TafDecoder`
        let mut buffers = TafBuffers::new();
        let mut page = [0u8; PAGE_SIZE];
        let mut direct = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        let mut scratch = [0u8; MAX_PACKET];
        direct.next_packet(&mut scratch).unwrap(); // OpusHead
        direct.next_packet(&mut scratch).unwrap(); // OpusTags
        direct.next_packet(&mut scratch).unwrap(); // first audio packet
        let mut expected = [0u8; MAX_PACKET];
        let expected_len = direct.next_packet(&mut expected).unwrap().unwrap();

        /// Fails to decode the first packet it sees, then records the next.
        struct FlakyDecoder {
            calls: usize,
            second_packet: Option<([u8; MAX_PACKET], usize)>,
        }
        impl OpusDecode for FlakyDecoder {
            fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError> {
                self.calls += 1;
                if self.calls == 1 {
                    return Err(AudioError::Decode);
                }
                let mut buf = [0u8; MAX_PACKET];
                buf[..packet.len()].copy_from_slice(packet);
                self.second_packet = Some((buf, packet.len()));
                let n = 960 * CHANNELS;
                pcm[..n].fill(0);
                Ok(n)
            }
        }
        let mut dec = open(
            FIXTURE,
            FlakyDecoder {
                calls: 0,
                second_packet: None,
            },
            &mut buffers,
        );
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        // When
        let failed = dec.next_frame(&mut pcm);
        let resumed = dec.next_frame(&mut pcm).unwrap();

        // Then
        assert_eq!(failed, Err(AudioError::Decode));
        assert!(resumed.is_some());
        let (buf, len) = dec
            .decoder
            .second_packet
            .expect("second call should have decoded a packet");
        assert_eq!(
            &buf[..len],
            &expected[..expected_len],
            "a decode error should consume the failed packet, so the next \
             call must resume at the following packet, not repeat it"
        );
    }

    #[test]
    fn seeking_to_chapter_zero_decodes_its_first_real_audio_packet_not_opus_head() {
        // Given: chapter 0 begins in the same block as OpusHead and OpusTags,
        // so the headers must be recognised by their magic bytes, not by
        // position. Its first real audio packet, read without `TafDecoder`.
        let mut buffers = TafBuffers::new();
        let mut page = [0u8; PAGE_SIZE];
        let mut direct = TafReader::open(SlicePages::new(FIXTURE).unwrap(), &mut page).unwrap();
        direct.seek_to_chapter(0).unwrap();
        let mut expected = [0u8; MAX_PACKET];
        let expected_len;
        loop {
            match direct.next_packet(&mut expected).unwrap() {
                None => panic!("unexpected end of stream"),
                Some(len)
                    if expected[..len].starts_with(b"OpusHead")
                        || expected[..len].starts_with(b"OpusTags") =>
                {
                    continue;
                }
                Some(len) => {
                    expected_len = len;
                    break;
                }
            }
        }

        /// Records the first packet it is asked to decode.
        struct CapturingDecoder {
            first_packet: Option<([u8; MAX_PACKET], usize)>,
        }
        impl OpusDecode for CapturingDecoder {
            fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError> {
                if self.first_packet.is_none() {
                    let mut buf = [0u8; MAX_PACKET];
                    buf[..packet.len()].copy_from_slice(packet);
                    self.first_packet = Some((buf, packet.len()));
                }
                let n = 960 * CHANNELS;
                pcm[..n].fill(0);
                Ok(n)
            }
        }
        let mut dec = open(
            FIXTURE,
            CapturingDecoder { first_packet: None },
            &mut buffers,
        );

        // When
        dec.seek_to_chapter(0).unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];
        dec.next_frame(&mut pcm).unwrap();

        // Then
        let (buf, len) = dec.decoder.first_packet.expect("decode was called");
        assert_eq!(
            &buf[..len],
            &expected[..expected_len],
            "chapter 0 begins at the same container block as OpusHead and OpusTags; \
             seeking there must decode the first real audio packet, identified by \
             magic (OpusHead/OpusTags), not by positional skip logic"
        );
    }
}
