//! The only place that knows the libopus API.

use core::ffi::c_int;

use crate::{AudioError, OpusDecode, CHANNELS, SAMPLE_RATE};

use teddiebox_opus_sys as sys;

/// Bytes reserved for a 48 kHz stereo decoder.
///
/// libopus needs 27 124 bytes on a 64-bit host, and less on the 32-bit
/// device, where pointers are smaller. The host size is therefore safe for
/// both; the device wastes a few kilobytes. Most of it is CELT's decode
/// history (2048 samples × 2 channels × 4 bytes = 16 KB), which cannot be
/// reduced.
///
/// [`LibOpus::new`] checks this against what libopus asks for, so a codec
/// configuration change cannot silently reserve too little.
pub const OPUS_STATE_BYTES: usize = 28 * 1024;

/// Storage for one libopus decoder, owned by the caller.
///
/// libopus initialises its state in place, so the firmware needs no heap. The
/// caller provides the 27 KB: put it in a `static`, not on an embassy task
/// stack, which is too small.
#[repr(C, align(8))]
pub struct OpusState {
    bytes: [u8; OPUS_STATE_BYTES],
}

impl OpusState {
    /// Reserves the storage. Cheap and const, so it can initialise a `static`.
    pub const fn new() -> Self {
        Self {
            bytes: [0; OPUS_STATE_BYTES],
        }
    }

    fn as_decoder_ptr(&mut self) -> *mut sys::OpusDecoder {
        self.bytes.as_mut_ptr().cast()
    }
}

impl Default for OpusState {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes Opus packets with libopus, using state the caller placed.
pub struct LibOpus<'a> {
    state: &'a mut OpusState,
}

impl<'a> LibOpus<'a> {
    /// Initialises a 48 kHz stereo decoder in `state`.
    ///
    /// TAF is always 48 kHz stereo, so nothing here is configurable.
    pub fn new(state: &'a mut OpusState) -> Result<Self, AudioError> {
        let needed = unsafe { sys::opus_decoder_get_size(CHANNELS as c_int) };
        if needed < 0 || needed as usize > OPUS_STATE_BYTES {
            // Cannot happen while `the_reservation_covers_what_libopus_asks_for`
            // passes. If it does, the codec configuration changed and
            // OPUS_STATE_BYTES must be measured again.
            return Err(AudioError::Decode);
        }

        // SAFETY: `bytes` is `needed` bytes or more, and 8-aligned, which
        // covers libopus's `int` and pointer alignment on both targets.
        // 48 000 Hz and 2 channels are both values libopus accepts.
        let rc = unsafe {
            sys::opus_decoder_init(
                state.as_decoder_ptr(),
                SAMPLE_RATE as i32,
                CHANNELS as c_int,
            )
        };
        if rc != sys::OPUS_OK {
            return Err(AudioError::Decode);
        }

        // Moving `LibOpus` afterwards is sound: libopus reaches its SILK and
        // CELT sub-states through byte offsets rather than self-pointers, and
        // the borrow keeps the state itself where the caller put it.
        Ok(Self { state })
    }
}

impl OpusDecode for LibOpus<'_> {
    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError> {
        let len = i32::try_from(packet.len()).map_err(|_| AudioError::Decode)?;
        let frame_size = c_int::try_from(pcm.len() / CHANNELS).map_err(|_| AudioError::Decode)?;

        // SAFETY: the state was initialised by `new`. `packet` is valid for
        // `len` bytes and `pcm` writable for `frame_size * CHANNELS` samples,
        // which is what `frame_size` was derived from.
        let n = unsafe {
            sys::opus_decode(
                self.state.as_decoder_ptr(),
                packet.as_ptr(),
                len,
                pcm.as_mut_ptr(),
                frame_size,
                0,
            )
        };

        if n < 0 {
            return Err(AudioError::Decode);
        }
        // libopus counts samples per channel; the trait returns the
        // interleaved total.
        Ok(n as usize * CHANNELS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teddiebox_taf::{SlicePages, TafReader, MAX_PACKET};

    const FIXTURE: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/sine.taf");

    #[test]
    fn the_reservation_covers_what_libopus_asks_for() {
        let needed = unsafe { sys::opus_decoder_get_size(CHANNELS as c_int) };
        assert!(
            needed > 0 && (needed as usize) <= OPUS_STATE_BYTES,
            "libopus wants {needed} bytes of decoder state, reserved {OPUS_STATE_BYTES}"
        );
    }

    #[test]
    fn decodes_the_first_real_audio_packet_to_a_full_stereo_frame() {
        let mut reader = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut scratch = [0u8; MAX_PACKET];
        reader.next_packet(&mut scratch).unwrap(); // OpusHead
        reader.next_packet(&mut scratch).unwrap(); // OpusTags
        let len = reader.next_packet(&mut scratch).unwrap().unwrap();

        let mut state = OpusState::new();
        let mut decoder = LibOpus::new(&mut state).unwrap();
        let mut pcm = [0i16; crate::MAX_FRAME_SAMPLES];
        let n = decoder.decode(&scratch[..len], &mut pcm).unwrap();

        // 60 ms of stereo audio at 48 kHz: 2880 samples/channel x 2 channels.
        // Uses real libopus, to check the per-channel to interleaved
        // conversion.
        assert_eq!(n, 5760);
    }

    #[test]
    fn a_corrupt_packet_is_a_decode_error_rather_than_a_panic() {
        let mut state = OpusState::new();
        let mut decoder = LibOpus::new(&mut state).unwrap();
        let mut pcm = [0i16; crate::MAX_FRAME_SAMPLES];

        // A TOC byte claiming a configuration the payload cannot support.
        let err = decoder.decode(&[0xff, 0xff, 0xff], &mut pcm).unwrap_err();

        assert_eq!(err, AudioError::Decode);
    }
}
