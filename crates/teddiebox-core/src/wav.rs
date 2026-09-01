//! Just enough WAV to play one from the SD card.
//!
//! Bench step 8 joins steps 6 and 7: a file read off the card and pushed at the
//! codec, for several minutes, without a gap. It uses WAV rather than TAF
//! deliberately — the point of the step is the path from card to I2S, and a
//! decoder in the middle would make an underrun and a decode fault look alike.
//!
//! Chunks are walked rather than assumed. The canonical header is 44 bytes and
//! `data` almost always starts there, but the format permits `LIST` or `fact`
//! chunks in between, and a parser that trusts offset 44 reads metadata as
//! audio — which sounds exactly like a driver fault.

/// The only format code that means plain samples.
const PCM_FORMAT: u16 = 1;

/// What step 6 configured the I2S peripheral and the codec for.
const CODEC_SAMPLE_RATE: u32 = 48_000;
const CODEC_CHANNELS: u16 = 2;
const CODEC_BITS: u16 = 16;

/// Why a file could not be played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavError {
    /// Not a RIFF/WAVE file at all.
    NotWave,
    /// The header ran out before `fmt ` and `data` were both found.
    Truncated,
    /// Compressed, or otherwise not plain PCM samples.
    NotPcm,
    /// A chunk header claims a size that cannot be true.
    Malformed,
}

/// What a WAV file says it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    /// Byte offset of the first sample.
    pub data_offset: u32,
    /// Length of the sample data in bytes.
    pub data_len: u32,
}

impl WavFormat {
    /// Reads the header from the front of a file.
    pub fn parse(bytes: &[u8]) -> Result<Self, WavError> {
        if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(WavError::NotWave);
        }

        let mut format: Option<(u16, u32, u16)> = None;
        let mut cursor = 12usize;

        loop {
            // A chunk header is an eight-byte tag and length. Anything less
            // left means the chunk we want was never there.
            if cursor + 8 > bytes.len() {
                return Err(WavError::Truncated);
            }
            let tag = &bytes[cursor..cursor + 4];
            let size = u32::from_le_bytes([
                bytes[cursor + 4],
                bytes[cursor + 5],
                bytes[cursor + 6],
                bytes[cursor + 7],
            ]);
            let body = cursor + 8;

            if tag == b"data" {
                let (channels, sample_rate, bits_per_sample) = format.ok_or(WavError::Truncated)?;
                return Ok(Self {
                    channels,
                    sample_rate,
                    bits_per_sample,
                    data_offset: body as u32,
                    data_len: size,
                });
            }

            if tag == b"fmt " {
                // A format chunk is at least sixteen bytes; extensible ones
                // are longer, and the extra fields are not needed here.
                if size < 16 {
                    return Err(WavError::Malformed);
                }
                if body + 16 > bytes.len() {
                    return Err(WavError::Truncated);
                }
                let code = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
                if code != PCM_FORMAT {
                    return Err(WavError::NotPcm);
                }
                format = Some((
                    u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]),
                    u32::from_le_bytes([
                        bytes[body + 4],
                        bytes[body + 5],
                        bytes[body + 6],
                        bytes[body + 7],
                    ]),
                    u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]),
                ));
            }

            // Odd-length chunks carry a pad byte that their declared size does
            // not count. Ignoring it reads every later chunk one byte out.
            let advance = (size as usize).checked_add(size as usize & 1);
            let next = advance
                .and_then(|padded| body.checked_add(padded))
                .ok_or(WavError::Malformed)?;
            if next > bytes.len() && tag != b"data" {
                return Err(WavError::Malformed);
            }
            cursor = next;
        }
    }

    /// Whether this is what the codec was configured for in step 6.
    ///
    /// The I2S peripheral is set up once, at boot, for 48 kHz stereo 16-bit.
    /// Anything else would play at the wrong speed rather than fail, which is
    /// a confusing thing to debug by ear.
    pub const fn matches_codec(&self) -> bool {
        self.channels == CODEC_CHANNELS
            && self.sample_rate == CODEC_SAMPLE_RATE
            && self.bits_per_sample == CODEC_BITS
    }

    /// How long the file plays for, in milliseconds.
    ///
    /// Step 8's criterion is "several minutes", so the bench needs to know
    /// whether the file it is about to play is long enough to prove anything.
    pub const fn duration_ms(&self) -> u32 {
        let bytes_per_frame = self.channels as u32 * (self.bits_per_sample as u32 / 8);
        if bytes_per_frame == 0 || self.sample_rate == 0 {
            return 0;
        }
        // Milliseconds first, so a long file does not overflow the frame count
        // before it is scaled down.
        (self.data_len / bytes_per_frame) / (self.sample_rate / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first 44 bytes of a real file written by `tools/taf2wav`, which is
    /// how any WAV on the bench card will have been made. Literal, from
    /// `xxd`, so this test can disagree with the parser.
    const REAL_HEADER: [u8; 44] = [
        0x52, 0x49, 0x46, 0x46, 0x24, 0x97, 0x0e, 0x00, // "RIFF", size 956196
        0x57, 0x41, 0x56, 0x45, // "WAVE"
        0x66, 0x6d, 0x74, 0x20, 0x10, 0x00, 0x00, 0x00, // "fmt ", size 16
        0x01, 0x00, // PCM
        0x02, 0x00, // 2 channels
        0x80, 0xbb, 0x00, 0x00, // 48000
        0x00, 0xee, 0x02, 0x00, // 192000 bytes/s
        0x04, 0x00, // block align 4
        0x10, 0x00, // 16 bits
        0x64, 0x61, 0x74, 0x61, 0x00, 0x97, 0x0e, 0x00, // "data", size 956160
    ];

    /// The first 44 bytes of a file from `scripts/make-test-wav.py`, which is
    /// what step 8 actually plays. A second writer, so the parser is pinned
    /// against both tools that produce WAVs for this project rather than
    /// against one of them.
    const GENERATED_HEADER: [u8; 44] = [
        0x52, 0x49, 0x46, 0x46, 0x24, 0xe8, 0x6e, 0x03, // "RIFF", size 57600036
        0x57, 0x41, 0x56, 0x45, // "WAVE"
        0x66, 0x6d, 0x74, 0x20, 0x10, 0x00, 0x00, 0x00, // "fmt ", size 16
        0x01, 0x00, 0x02, 0x00, // PCM, 2 channels
        0x80, 0xbb, 0x00, 0x00, // 48000
        0x00, 0xee, 0x02, 0x00, // 192000 bytes/s
        0x04, 0x00, 0x10, 0x00, // block align 4, 16 bits
        0x64, 0x61, 0x74, 0x61, 0x00, 0xe8, 0x6e, 0x03, // "data", size 57600000
    ];

    /// Step 8 asks for "several minutes". Five, and the parser agrees with the
    /// generator about it — the two compute it from opposite ends.
    #[test]
    fn the_step_8_test_file_is_five_minutes_the_codec_can_play() {
        let format = WavFormat::parse(&GENERATED_HEADER).expect("the generator's header parses");
        assert!(format.matches_codec());
        assert_eq!(format.data_offset, 44);
        assert_eq!(format.duration_ms(), 300_000);
    }

    #[test]
    fn a_real_file_from_taf2wav_is_read_correctly() {
        let format = WavFormat::parse(&REAL_HEADER).expect("a real header parses");
        assert_eq!(format.channels, 2);
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.bits_per_sample, 16);
        assert_eq!(format.data_offset, 44);
        assert_eq!(format.data_len, 956_160);
    }

    #[test]
    fn a_real_file_matches_what_the_codec_was_configured_for() {
        let format = WavFormat::parse(&REAL_HEADER).unwrap();
        assert!(format.matches_codec());
    }

    /// 956160 bytes / 4 bytes per frame / 48000 frames per second.
    #[test]
    fn the_duration_comes_from_the_data_length() {
        let format = WavFormat::parse(&REAL_HEADER).unwrap();
        assert_eq!(format.duration_ms(), 4980);
    }

    /// The trap this parser exists for: `data` need not be at offset 44.
    /// Trusting that offset reads a metadata chunk as audio, which sounds like
    /// a broken driver rather than like a mis-parsed file.
    /// `RIFF` + size + `WAVE`, then an interloping chunk, then the real
    /// `fmt ` and `data` headers from the file above.
    fn with_leading_chunk(chunk: &[u8]) -> [u8; 64] {
        let mut file = [0u8; 64];
        file[..12].copy_from_slice(&REAL_HEADER[..12]);
        file[12..12 + chunk.len()].copy_from_slice(chunk);
        let tail = 12 + chunk.len();
        file[tail..tail + 32].copy_from_slice(&REAL_HEADER[12..]);
        file
    }

    #[test]
    fn a_chunk_before_the_format_chunk_is_stepped_over() {
        // "LIST", size 4, "INFO"
        let file = with_leading_chunk(b"LIST\x04\x00\x00\x00INFO");

        let format = WavFormat::parse(&file[..56]).expect("LIST is skipped");
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.data_offset, 56, "12 bytes further on than usual");
    }

    /// RIFF pads odd-sized chunks to an even boundary, and the pad byte is not
    /// counted in the chunk's declared size. Miss it and every later chunk is
    /// read one byte out of step.
    #[test]
    fn an_odd_sized_chunk_is_padded_to_an_even_boundary() {
        // "LIST", size 3, "abc", then one pad byte that the size does not count
        let file = with_leading_chunk(b"LIST\x03\x00\x00\x00abc\x00");

        let format = WavFormat::parse(&file[..56]).expect("the pad byte is accounted for");
        assert_eq!(format.sample_rate, 48_000);
    }

    #[test]
    fn a_file_that_is_not_riff_is_rejected() {
        let mut file = REAL_HEADER;
        file[0] = b'X';
        assert_eq!(WavFormat::parse(&file), Err(WavError::NotWave));
    }

    #[test]
    fn a_riff_file_that_is_not_wave_is_rejected() {
        let mut file = REAL_HEADER;
        file[8] = b'X';
        assert_eq!(WavFormat::parse(&file), Err(WavError::NotWave));
    }

    #[test]
    fn a_header_cut_short_is_an_error_rather_than_a_guess() {
        assert_eq!(
            WavFormat::parse(&REAL_HEADER[..20]),
            Err(WavError::Truncated)
        );
        assert_eq!(WavFormat::parse(&[]), Err(WavError::NotWave));
    }

    /// Compressed WAVs exist. Playing one as PCM is loud noise, so it is worth
    /// naming rather than discovering through the speaker.
    #[test]
    fn a_file_that_is_not_plain_pcm_is_rejected() {
        let mut file = REAL_HEADER;
        file[20] = 0x11; // IMA ADPCM
        assert_eq!(WavFormat::parse(&file), Err(WavError::NotPcm));
    }

    #[test]
    fn a_chunk_claiming_an_impossible_size_is_rejected() {
        let mut file = REAL_HEADER;
        file[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(WavFormat::parse(&file), Err(WavError::Malformed));
    }

    /// Mono, or 44.1 kHz, would play at the wrong speed rather than fail. The
    /// file is still well formed, so this is a separate question from parsing.
    #[test]
    fn a_file_the_codec_was_not_configured_for_parses_but_does_not_match() {
        let mut file = REAL_HEADER;
        file[22] = 1; // mono
        let format = WavFormat::parse(&file).expect("still a valid WAV");
        assert_eq!(format.channels, 1);
        assert!(!format.matches_codec());
    }
}
