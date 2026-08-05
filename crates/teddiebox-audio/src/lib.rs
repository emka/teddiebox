#![no_std]

mod opus;
pub use opus::LibOpus;

use teddiebox_taf::{PageSource, TafError, TafReader, MAX_PACKET};

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

/// Decodes one Opus packet into interleaved 16-bit PCM.
pub trait OpusDecode {
    /// Returns the number of samples written (frames × channels).
    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError>;
}

pub struct TafDecoder<S: PageSource, D: OpusDecode> {
    reader: TafReader<S>,
    decoder: D,
    packet: [u8; MAX_PACKET],
    headers_skipped: bool,
}

impl<S: PageSource, D: OpusDecode> TafDecoder<S, D> {
    pub fn new(reader: TafReader<S>, decoder: D) -> Self {
        Self {
            reader,
            decoder,
            packet: [0u8; MAX_PACKET],
            headers_skipped: false,
        }
    }

    pub fn reader(&mut self) -> &mut TafReader<S> {
        &mut self.reader
    }

    /// Decodes the next audio packet. `Ok(None)` at end of stream.
    pub fn next_frame(&mut self, pcm: &mut [i16]) -> Result<Option<usize>, AudioError> {
        if pcm.len() < MAX_FRAME_SAMPLES {
            return Err(AudioError::BufferTooSmall);
        }

        // OpusHead and OpusTags precede the audio and must not reach libopus.
        if !self.headers_skipped {
            for _ in 0..2 {
                let mut scratch = [0u8; MAX_PACKET];
                if self.reader.next_packet(&mut scratch)?.is_none() {
                    return Ok(None);
                }
            }
            self.headers_skipped = true;
        }

        match self.reader.next_packet(&mut self.packet)? {
            None => Ok(None),
            Some(len) => {
                let n = self.decoder.decode(&self.packet[..len], pcm)?;
                Ok(Some(n))
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
        let reader = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut dec = TafDecoder::new(
            reader,
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        );
        let mut pcm = [0i16; MAX_FRAME_SAMPLES];

        dec.next_frame(&mut pcm).unwrap();
        assert!(!dec.decoder.saw_opus_header);
    }

    #[test]
    fn decodes_every_audio_packet_then_reports_end_of_stream() {
        let reader = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut dec = TafDecoder::new(
            reader,
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        );
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
        let reader = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut dec = TafDecoder::new(
            reader,
            StubDecoder {
                calls: 0,
                saw_opus_header: false,
            },
        );
        let mut pcm = [0i16; 8];
        assert_eq!(dec.next_frame(&mut pcm), Err(AudioError::BufferTooSmall));
    }
}
