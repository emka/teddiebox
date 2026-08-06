#![no_std]

mod opus;
pub use opus::LibOpus;

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

pub struct TafDecoder<S: PageSource, D: OpusDecode> {
    reader: TafReader<S>,
    decoder: D,
    /// Fixed-size: see `MAX_PACKET`'s doc for what size was chosen and why
    /// a packet that doesn't fit here is unrecoverable, not just this call's
    /// problem.
    packet: [u8; MAX_PACKET],
    /// Set once `next_packet` reports `TafError::BufferTooSmall` against
    /// `packet`. `packet`'s size is fixed for the lifetime of this decoder,
    /// so the error is terminal *at the reader's current position*: without
    /// an intervening seek, every later call must keep reporting the same
    /// error instead of re-asking the reader for a packet it already told
    /// us doesn't fit. A successful `seek_to_chapter` clears the flag,
    /// because it moves the reader to a different packet, which may well
    /// fit the buffer even though the one that tripped this didn't.
    buffer_exhausted: bool,
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
            buffer_exhausted: false,
        })
    }

    pub fn header(&self) -> &TonieHeader {
        self.reader.header()
    }

    pub fn chapter_count(&self) -> usize {
        self.reader.chapter_count()
    }

    /// Seeks to a chapter and resumes decoding from there.
    ///
    /// Clears a latched `BufferTooSmall` on success: that error is terminal
    /// only for the packet at the position that produced it, and a
    /// successful seek moves to a different position. On failure, leaves
    /// `buffer_exhausted` (and every other field) untouched, mirroring
    /// `TafReader::seek_to_chapter`'s own contract that a failed seek must
    /// not change where the reader is positioned.
    pub fn seek_to_chapter(&mut self, n: usize) -> Result<(), AudioError> {
        self.reader.seek_to_chapter(n)?;
        self.buffer_exhausted = false;
        Ok(())
    }

    /// Decodes the next audio packet. `Ok(None)` at end of stream.
    ///
    /// Retry semantics differ by error, and callers need to know which they
    /// got. A [`TafError::BufferTooSmall`] wrapped in [`AudioError::Container`]
    /// is the one exception to "container errors are retryable" *at a fixed
    /// position*: `packet` is a fixed-size buffer, so the packet at the
    /// reader's current position that didn't fit it never will, however many
    /// times this is called without an intervening seek. Every such call
    /// reports that same error immediately without asking the reader again.
    /// It is not permanent, though: [`Self::seek_to_chapter`] moves the
    /// reader to a different packet, which may well fit, so a successful
    /// seek clears it. Any other [`AudioError::Container`] leaves the packet unconsumed, so
    /// retrying re-attempts it. A [`AudioError::Decode`] arrives *after* the
    /// packet has been taken from the container, so that frame's audio is
    /// gone: a retry resumes at the following packet. Treat a decode error
    /// as a dropped frame, not as a repeatable failure.
    pub fn next_frame(&mut self, pcm: &mut [i16]) -> Result<Option<usize>, AudioError> {
        if pcm.len() < MAX_FRAME_SAMPLES {
            return Err(AudioError::BufferTooSmall);
        }
        if self.buffer_exhausted {
            return Err(AudioError::Container(TafError::BufferTooSmall));
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
                Err(TafError::BufferTooSmall) => {
                    self.buffer_exhausted = true;
                    return Err(AudioError::Container(TafError::BufferTooSmall));
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

    /// Builds a minimal TAF header page declaring `data_length` bytes of
    /// stream, mirroring `teddiebox-taf`'s own private test helper of the
    /// same name (duplicated here rather than reused, since it's private to
    /// that crate).
    fn header_page(data_length: u32) -> [u8; teddiebox_taf::PAGE_SIZE] {
        let mut page = [0u8; teddiebox_taf::PAGE_SIZE];
        // field 2 (data_length), wire type 0 (varint): key byte 0x10,
        // followed by however many varint bytes `data_length` needs.
        let mut fields = [0u8; 8];
        let mut n = 0usize;
        fields[n] = 0x10;
        n += 1;
        let mut v = data_length;
        loop {
            let mut byte = (v & 0x7F) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            fields[n] = byte;
            n += 1;
            if v == 0 {
                break;
            }
        }
        page[0..4].copy_from_slice(&(n as u32).to_be_bytes());
        page[4..4 + n].copy_from_slice(&fields[..n]);
        page
    }

    #[test]
    fn a_packet_too_large_for_the_decoders_fixed_buffer_is_a_terminal_error() {
        // One packet larger than `MAX_PACKET`: `TafDecoder` has no bigger
        // buffer to retry with, however many times it's called, unlike a
        // caller that owns a resizable buffer.
        let packet_len = MAX_PACKET + 25;
        let mut ogg_page = [0u8; teddiebox_taf::PAGE_SIZE];
        ogg_page[0..4].copy_from_slice(b"OggS");
        let mut off = 27usize;
        let mut remaining = packet_len;
        while remaining >= 255 {
            ogg_page[off] = 255;
            off += 1;
            remaining -= 255;
        }
        ogg_page[off] = remaining as u8;
        off += 1;
        ogg_page[26] = (off - 27) as u8;
        let payload_start = off;
        ogg_page[payload_start..payload_start + packet_len].fill(0xAB);

        let mut file = [0u8; teddiebox_taf::PAGE_SIZE * 2];
        file[..teddiebox_taf::PAGE_SIZE]
            .copy_from_slice(&header_page(teddiebox_taf::PAGE_SIZE as u32));
        file[teddiebox_taf::PAGE_SIZE..].copy_from_slice(&ogg_page);

        let mut dec = TafDecoder::open(
            SlicePages::new(&file).unwrap(),
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        )
        .unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        assert_eq!(
            dec.next_frame(&mut pcm),
            Err(AudioError::Container(TafError::BufferTooSmall))
        );
        // Terminal: a second call must report the same error again, not
        // panic, not decode stale bytes, and not hang retrying a buffer
        // size that will never change.
        assert_eq!(
            dec.next_frame(&mut pcm),
            Err(AudioError::Container(TafError::BufferTooSmall))
        );
        assert_eq!(dec.decoder.calls, 0, "decode must never be reached");
    }

    #[test]
    fn seeking_after_a_buffer_too_small_error_recovers_at_the_new_chapter() {
        // Reproduces the regression this test guards against: once some
        // packet trips the decoder's terminal `BufferTooSmall`, seeking to a
        // *different* chapter whose own packets fit fine must still recover,
        // rather than keep reporting the old, position-specific error forever.
        const CHAPTERS_FIXTURE: &[u8] =
            include_bytes!("../../teddiebox-taf/tests/data/chapters.taf");

        // Corrupt file page 1 (chapter 0's start, right after the header
        // page) with a single packet larger than `MAX_PACKET`, following the
        // same construction as
        // `a_packet_too_large_for_the_decoders_fixed_buffer_is_a_terminal_error`.
        // Chapter 1 (file page 6, ogg page 5) is untouched real audio and
        // still fits the buffer.
        let packet_len = MAX_PACKET + 25;
        let mut ogg_page = [0u8; teddiebox_taf::PAGE_SIZE];
        ogg_page[0..4].copy_from_slice(b"OggS");
        let mut off = 27usize;
        let mut remaining = packet_len;
        while remaining >= 255 {
            ogg_page[off] = 255;
            off += 1;
            remaining -= 255;
        }
        ogg_page[off] = remaining as u8;
        off += 1;
        ogg_page[26] = (off - 27) as u8;
        let payload_start = off;
        ogg_page[payload_start..payload_start + packet_len].fill(0xAB);

        let mut file = [0u8; teddiebox_taf::PAGE_SIZE * 17];
        file.copy_from_slice(CHAPTERS_FIXTURE);
        file[teddiebox_taf::PAGE_SIZE..teddiebox_taf::PAGE_SIZE * 2].copy_from_slice(&ogg_page);

        let mut dec = TafDecoder::open(
            SlicePages::new(&file).unwrap(),
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        )
        .unwrap();
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        assert_eq!(
            dec.next_frame(&mut pcm),
            Err(AudioError::Container(TafError::BufferTooSmall))
        );

        dec.seek_to_chapter(1)
            .expect("chapter 1's own packets fit; the seek itself must succeed");

        assert!(
            dec.next_frame(&mut pcm).unwrap().is_some(),
            "a seek to a chapter whose packets fit must clear the terminal \
             BufferTooSmall left over from a different position in the file"
        );
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
