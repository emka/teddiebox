//! Just enough WAV parsing to play a file from the SD card.
//!
//! Used to test the path from card to I2S without a decoder in between, so an
//! underrun cannot be confused with a decode fault.
//!
//! Chunks are walked, not assumed. The standard header is 44 bytes and `data`
//! almost always starts there, but `LIST` or `fact` chunks may come first. A
//! parser that assumed offset 44 would play metadata as audio.

/// The only format code that means plain samples.
const PCM_FORMAT: u16 = 1;

/// The format the I2S peripheral and the codec are configured for.
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
            // A chunk header is an eight-byte tag and length. Fewer bytes
            // left means the chunk we want is missing.
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

            // Odd-length chunks have a pad byte not counted in their size.
            // Ignoring it would read every later chunk one byte off.
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

    /// Whether this matches the codec's configuration.
    ///
    /// The I2S peripheral is set up once, at boot, for 48 kHz stereo 16-bit.
    /// Anything else would play at the wrong speed instead of failing.
    pub const fn matches_codec(&self) -> bool {
        self.channels == CODEC_CHANNELS
            && self.sample_rate == CODEC_SAMPLE_RATE
            && self.bits_per_sample == CODEC_BITS
    }

    /// How long the file plays for, in milliseconds.
    ///
    /// Used to check a test file is long enough. Zero for a header whose
    /// numbers cannot describe a playable file.
    pub const fn duration_ms(&self) -> u32 {
        let bytes_per_frame = self.channels as u32 * (self.bits_per_sample as u32 / 8);
        // Divide instead of multiplying by 1000, so a long file cannot
        // overflow.
        let frames_per_ms = self.sample_rate / 1000;
        if bytes_per_frame == 0 || frames_per_ms == 0 {
            return 0;
        }
        (self.data_len / bytes_per_frame) / frames_per_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first 44 bytes of a real file written by `tools/taf2wav`, copied
    /// from `xxd` so the test can disagree with the parser.
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

    /// The first 44 bytes of a file from `scripts/make-test-wav.py`, the
    /// other tool that writes WAVs for this project.
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

    /// The generated test file is five minutes long.
    #[test]
    fn the_generated_test_file_is_five_minutes_the_codec_can_play() {
        // Given
        let header = GENERATED_HEADER;

        // When
        let format = WavFormat::parse(&header).expect("the generator's header parses");

        // Then
        assert!(format.matches_codec());
        assert_eq!(format.data_offset, 44);
        assert_eq!(format.duration_ms(), 300_000);
    }

    #[test]
    fn a_real_file_from_taf2wav_is_read_correctly() {
        // Given
        let header = REAL_HEADER;

        // When
        let format = WavFormat::parse(&header);

        // Then
        assert_eq!(
            format,
            Ok(WavFormat {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 16,
                data_offset: 44,
                data_len: 956_160,
            })
        );
    }

    #[test]
    fn a_real_file_matches_what_the_codec_was_configured_for() {
        // Given
        let format = WavFormat::parse(&REAL_HEADER).unwrap();

        // When
        let matches = format.matches_codec();

        // Then
        assert!(matches);
    }

    /// 956160 bytes / 4 bytes per frame / 48000 frames per second.
    #[test]
    fn the_duration_comes_from_the_data_length() {
        // Given
        let format = WavFormat::parse(&REAL_HEADER).unwrap();

        // When
        let duration = format.duration_ms();

        // Then
        assert_eq!(duration, 4980);
    }

    /// The firmware logs the duration of any WAV it finds before checking
    /// that the codec can play it, so a nonsense rate must not panic.
    #[test]
    fn a_rate_below_one_kilohertz_has_no_duration() {
        // Given
        let mut file = REAL_HEADER;
        file[24..28].copy_from_slice(&500u32.to_le_bytes());
        let format = WavFormat::parse(&file).expect("still a valid WAV");

        // When
        let duration = format.duration_ms();

        // Then
        assert_eq!(duration, 0);
    }

    /// A file with an extra chunk before `fmt `, so `data` is not at offset
    /// 44: `RIFF` + size + `WAVE`, then `chunk`, then the real `fmt ` and
    /// `data` headers from the file above.
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
        // Given: "LIST", size 4, "INFO"
        let file = with_leading_chunk(b"LIST\x04\x00\x00\x00INFO");

        // When
        let format = WavFormat::parse(&file[..56]).expect("LIST is skipped");

        // Then
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.data_offset, 56, "12 bytes further on than usual");
    }

    /// RIFF pads odd-sized chunks to an even length, and the pad byte is not
    /// counted in the chunk's size.
    #[test]
    fn an_odd_sized_chunk_is_padded_to_an_even_boundary() {
        // Given: "LIST", size 3, "abc", then one pad byte that the size does
        // not count
        let file = with_leading_chunk(b"LIST\x03\x00\x00\x00abc\x00");

        // When
        let format = WavFormat::parse(&file[..56]).expect("the pad byte is accounted for");

        // Then
        assert_eq!(format.sample_rate, 48_000);
    }

    #[test]
    fn a_file_that_is_not_riff_is_rejected() {
        // Given
        let mut file = REAL_HEADER;
        file[0] = b'X';

        // When
        let format = WavFormat::parse(&file);

        // Then
        assert_eq!(format, Err(WavError::NotWave));
    }

    #[test]
    fn a_riff_file_that_is_not_wave_is_rejected() {
        // Given
        let mut file = REAL_HEADER;
        file[8] = b'X';

        // When
        let format = WavFormat::parse(&file);

        // Then
        assert_eq!(format, Err(WavError::NotWave));
    }

    #[test]
    fn a_header_cut_short_is_an_error_rather_than_a_guess() {
        // Given
        let cut_short: [&[u8]; 2] = [&REAL_HEADER[..20], &[]];

        // When
        let formats = cut_short.map(WavFormat::parse);

        // Then
        assert_eq!(formats, [Err(WavError::Truncated), Err(WavError::NotWave)]);
    }

    /// Playing a compressed WAV as PCM would be loud noise.
    #[test]
    fn a_file_that_is_not_plain_pcm_is_rejected() {
        // Given
        let mut file = REAL_HEADER;
        file[20] = 0x11; // IMA ADPCM

        // When
        let format = WavFormat::parse(&file);

        // Then
        assert_eq!(format, Err(WavError::NotPcm));
    }

    #[test]
    fn a_chunk_claiming_an_impossible_size_is_rejected() {
        // Given
        let mut file = REAL_HEADER;
        file[16..20].copy_from_slice(&u32::MAX.to_le_bytes());

        // When
        let format = WavFormat::parse(&file);

        // Then
        assert_eq!(format, Err(WavError::Malformed));
    }

    /// The fields read from a format chunk take sixteen bytes. A shorter chunk
    /// would have them read from whatever follows it.
    #[test]
    fn a_format_chunk_too_short_for_its_fields_is_rejected() {
        // Given: a format chunk that ends before the sample size, followed
        // directly by the data chunk
        let mut file = [0u8; 42];
        file[..12].copy_from_slice(&REAL_HEADER[..12]);
        file[12..16].copy_from_slice(b"fmt ");
        file[16..20].copy_from_slice(&14u32.to_le_bytes());
        file[20..34].copy_from_slice(&REAL_HEADER[20..34]);
        file[34..].copy_from_slice(&REAL_HEADER[36..]);

        // When
        let format = WavFormat::parse(&file);

        // Then
        assert_eq!(format, Err(WavError::Malformed));
    }

    /// Mono, 44.1 kHz or 8-bit samples would play at the wrong speed or as
    /// noise. The file is still valid, so parsing succeeds and only the codec
    /// check fails.
    #[test]
    fn a_file_the_codec_was_not_configured_for_parses_but_does_not_match() {
        // Given
        let mut mono = REAL_HEADER;
        mono[22] = 1;
        let mut cd_rate = REAL_HEADER;
        cd_rate[24..28].copy_from_slice(&44_100u32.to_le_bytes());
        let mut eight_bit = REAL_HEADER;
        eight_bit[34] = 8;

        // When
        let matches = [mono, cd_rate, eight_bit]
            .map(|file| WavFormat::parse(&file).map(|format| format.matches_codec()));

        // Then
        assert_eq!(matches, [Ok(false); 3]);
    }
}
