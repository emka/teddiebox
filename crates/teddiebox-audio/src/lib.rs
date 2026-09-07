#![no_std]

mod opus;
pub use opus::{LibOpus, OpusState, OPUS_STATE_BYTES};

use teddiebox_taf::{PageSource, TafError, TafReader, TonieHeader, MAX_PACKET};

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

pub struct TafDecoder<S: PageSource, D: OpusDecode> {
    reader: TafReader<S>,
    decoder: D,
    /// Sized so that every packet a TAF file can contain fits — see
    /// [`MAX_PACKET`], which is derived from the page size rather than from
    /// the codec, so "too big for this buffer" is not a state this decoder
    /// can reach.
    packet: [u8; MAX_PACKET],
}

impl<S: PageSource, D: OpusDecode> TafDecoder<S, D> {
    /// Opens a source and takes ownership of the reader.
    ///
    /// Deliberately does **not** accept a pre-built `TafReader`, so that the
    /// container position and the decoder's own packet buffer cannot drift
    /// apart: a caller holding its own handle could advance the reader behind
    /// the decoder's back. Owning it from the start makes that unreachable
    /// rather than merely discouraged.
    ///
    /// Note this is no longer what protects the Opus header packets — those
    /// are identified by magic in [`Self::next_frame`], which is correct from
    /// any starting position.
    pub fn open(source: S, decoder: D) -> Result<Self, AudioError> {
        Ok(Self {
            reader: TafReader::open(source)?,
            decoder,
            packet: [0u8; MAX_PACKET],
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
    /// Delegated to the reader rather than latched on seek: the reader is the
    /// only thing that knows the stream advanced past a chapter start on its
    /// own, and a copy kept here would be wrong for the whole rest of any
    /// story played straight through.
    pub fn chapter(&self) -> usize {
        self.reader.current_chapter()
    }

    /// Seeks to a chapter and resumes decoding from there.
    ///
    /// A failed seek leaves the reader positioned exactly where it was,
    /// mirroring `TafReader::seek_to_chapter`'s own contract.
    pub fn seek_to_chapter(&mut self, n: usize) -> Result<(), AudioError> {
        self.reader.seek_to_chapter(n)?;
        Ok(())
    }

    /// Skips forward to the start of the following chapter.
    ///
    /// [`Skip::PastTheEnd`] from the last chapter, with the story left where
    /// it was: skipping out of the end of the last chapter is the same thing
    /// as the story finishing, and the caller ends it rather than playing
    /// that chapter a second time.
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
    /// Restarting rather than refusing, because a control that does nothing
    /// is indistinguishable from a control that is broken, and starting the
    /// story again is what going back from its beginning means.
    pub fn previous_chapter(&mut self) -> Result<usize, AudioError> {
        let previous = self.chapter().saturating_sub(1);
        self.seek_to_chapter(previous)?;
        Ok(previous)
    }

    /// Decodes the next audio packet. `Ok(None)` at end of stream.
    ///
    /// Retry semantics differ by error, and callers need to know which they
    /// got. An [`AudioError::Container`] leaves the packet unconsumed, so
    /// retrying re-attempts it. A [`AudioError::Decode`] arrives *after* the
    /// packet has been taken from the container, so that frame's audio is
    /// gone: a retry resumes at the following packet. Treat a decode error
    /// as a dropped frame, not as a repeatable failure.
    pub fn next_frame(&mut self, pcm: &mut [i16]) -> Result<Option<usize>, AudioError> {
        if pcm.len() < MAX_FRAME_SAMPLES {
            return Err(AudioError::BufferTooSmall);
        }
        // OpusHead and OpusTags must not reach libopus. Identify them by
        // their magic rather than by position: a positional skip is wrong
        // after a seek, because chapter 0 legitimately starts *at* OpusHead —
        // the headers and the first audio page share the first container
        // block. Skipping by position would drop real audio from chapter 0,
        // and a "already skipped" flag set on seek would feed OpusHead to
        // libopus as audio. Matching the magic is correct in every position.
        loop {
            match self.reader.next_packet(&mut self.packet) {
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

    #[test]
    fn the_opus_header_packets_are_never_sent_to_the_decoder() {
        let mut dec = TafDecoder::open(
            SlicePages::new(FIXTURE).unwrap(),
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        )
        .unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        // A single call only proves the *first* decoded packet isn't a
        // header -- the name says "never". Drive the decoder over the
        // whole stream, as the neighbouring test does, so the assertion
        // actually matches what the name claims.
        while dec.next_frame(&mut pcm).unwrap().is_some() {}
        assert!(!dec.decoder.saw_opus_header);
    }

    #[test]
    fn decodes_every_audio_packet_then_reports_end_of_stream() {
        let mut dec = TafDecoder::open(
            SlicePages::new(FIXTURE).unwrap(),
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        )
        .unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        let mut frames = 0;
        while dec.next_frame(&mut pcm).unwrap().is_some() {
            frames += 1;
        }
        // Measured from the real fixture: 85 packets total, of which the first
        // two are OpusHead and OpusTags, leaving 83 audio frames. `toniefile`
        // encodes 60 ms Opus frames (2880 samples/channel), not 20 ms —
        // 83 x 2880 = 239040 samples = 4.98 s.
        assert_eq!(frames, 83, "expected 83 audio frames, got {frames}");
        assert_eq!(dec.next_frame(&mut pcm).unwrap(), None);
    }

    #[test]
    fn rejects_a_pcm_buffer_that_is_too_small() {
        let mut dec = TafDecoder::open(
            SlicePages::new(FIXTURE).unwrap(),
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        )
        .unwrap();
        let mut pcm = [0i16; 8];
        assert_eq!(dec.next_frame(&mut pcm), Err(AudioError::BufferTooSmall));
    }

    /// The decode loop is the only thing that knows a story is playing, so a
    /// skip decided elsewhere has to be able to ask the decoder where it is.
    #[test]
    fn the_decoder_reports_the_chapter_it_is_playing() {
        const CHAPTERS_FIXTURE: &[u8] =
            include_bytes!("../../teddiebox-taf/tests/data/chapters.taf");
        let mut dec = TafDecoder::open(
            SlicePages::new(CHAPTERS_FIXTURE).unwrap(),
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        )
        .unwrap();
        assert_eq!(dec.chapter(), 0);
        dec.seek_to_chapter(2).unwrap();
        assert_eq!(dec.chapter(), 2);
    }

    /// Records the last packet it was asked to decode, so a test can say
    /// which part of the file the decoder actually went to.
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

    fn chapters_decoder() -> TafDecoder<SlicePages<'static>, LastPacket> {
        const CHAPTERS_FIXTURE: &[u8] =
            include_bytes!("../../teddiebox-taf/tests/data/chapters.taf");
        TafDecoder::open(
            SlicePages::new(CHAPTERS_FIXTURE).unwrap(),
            LastPacket {
                buf: [0u8; MAX_PACKET],
                len: 0,
            },
        )
        .unwrap()
    }

    #[test]
    fn the_next_chapter_is_the_one_after_the_chapter_playing() {
        let mut dec = chapters_decoder();
        assert_eq!(dec.next_chapter(), Ok(Skip::To(1)));
        assert_eq!(dec.chapter(), 1);
        assert_eq!(dec.next_chapter(), Ok(Skip::To(2)));
        assert_eq!(dec.chapter(), 2);
    }

    /// A child skipping forward out of the last chapter has reached the end
    /// of the story, which is the same thing as the story finishing. Saying
    /// so is what lets the decode loop end instead of playing the last
    /// chapter twice.
    #[test]
    fn there_is_no_chapter_after_the_last_one() {
        let mut dec = chapters_decoder();
        dec.seek_to_chapter(2).unwrap();
        assert_eq!(dec.next_chapter(), Ok(Skip::PastTheEnd));
        assert_eq!(dec.chapter(), 2, "a refused skip must not move the story");
    }

    #[test]
    fn the_previous_chapter_is_the_one_before_the_chapter_playing() {
        let mut dec = chapters_decoder();
        dec.seek_to_chapter(2).unwrap();
        assert_eq!(dec.previous_chapter(), Ok(1));
        assert_eq!(dec.chapter(), 1);
    }

    /// Going back from the first chapter restarts it rather than doing
    /// nothing. Nothing at all is indistinguishable from a control that did
    /// not work, and starting the story over is what the gesture means
    /// everywhere else it exists.
    #[test]
    fn going_back_from_the_first_chapter_starts_it_again() {
        let mut dec = chapters_decoder();
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

        assert_eq!(dec.previous_chapter(), Ok(0));
        dec.next_frame(&mut pcm).unwrap();
        assert_eq!(&dec.decoder.buf[..dec.decoder.len], &first[..first_len]);
    }

    #[test]
    fn seeking_to_a_chapter_decodes_its_first_packet_not_a_later_one() {
        const CHAPTERS_FIXTURE: &[u8] =
            include_bytes!("../../teddiebox-taf/tests/data/chapters.taf");

        // Independently determine what the first packet of chapter 1 actually
        // is, without going through `TafDecoder` at all.
        let mut direct = TafReader::open(SlicePages::new(CHAPTERS_FIXTURE).unwrap()).unwrap();
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

        let mut dec = TafDecoder::open(
            SlicePages::new(CHAPTERS_FIXTURE).unwrap(),
            CapturingDecoder { first_packet: None },
        )
        .unwrap();
        dec.seek_to_chapter(1).unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];
        dec.next_frame(&mut pcm).unwrap();

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
        // Independently determine what the second real audio packet is.
        let mut direct = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
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

        let mut dec = TafDecoder::open(
            SlicePages::new(FIXTURE).unwrap(),
            FlakyDecoder {
                calls: 0,
                second_packet: None,
            },
        )
        .unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        assert_eq!(dec.next_frame(&mut pcm), Err(AudioError::Decode));
        assert!(dec.next_frame(&mut pcm).unwrap().is_some());

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
        // Chapter 0 begins at the same container block as OpusHead and OpusTags,
        // so positional header skipping fails: it would either skip real audio
        // or feed OpusHead to the decoder depending on how the seek was entered.
        // This test ensures we identify headers by magic, not by position.

        // Independently determine what the first real audio packet of chapter 0 is.
        let mut direct = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        direct.seek_to_chapter(0).unwrap();
        let mut expected = [0u8; MAX_PACKET];
        let expected_len;
        // Skip OpusHead and OpusTags by their magic.
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

        let mut dec = TafDecoder::open(
            SlicePages::new(FIXTURE).unwrap(),
            CapturingDecoder { first_packet: None },
        )
        .unwrap();
        dec.seek_to_chapter(0).unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];
        dec.next_frame(&mut pcm).unwrap();

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
