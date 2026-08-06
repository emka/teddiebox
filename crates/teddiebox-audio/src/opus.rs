//! The only place that knows the libopus API.

use core::ffi::c_int;

use crate::{AudioError, OpusDecode, CHANNELS, SAMPLE_RATE};

use teddiebox_opus_sys as sys;

/// Bytes reserved for a 48 kHz stereo decoder.
///
/// libopus reports 27 124 bytes on a 64-bit host, and less on the 32-bit
/// device because every pointer and offset inside the state shrinks — so the
/// host figure is a safe reservation for both, and the device wastes a few
/// kilobytes until Phase B can measure the real number. Most of it is CELT's
/// decode history: 2048 samples × 2 channels × 4 bytes is 16 KB on its own
/// and cannot be traded away.
///
/// [`LibOpus::new`] checks this against what libopus asks for, so changing
/// the codec's configuration cannot silently under-reserve.
pub const OPUS_STATE_BYTES: usize = 28 * 1024;

/// Storage for one libopus decoder, owned by the caller.
///
/// libopus sizes its own state and initialises it in place, which is what
/// lets the firmware decode with no heap at all. The consequence is that
/// 27 KB has to come from somewhere the caller chooses: put this in a
/// `static`, not on an embassy task stack, which is smaller than the state.
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
            // Unreachable while `the_reservation_covers_what_libopus_asks_for`
            // passes: reaching it means the codec configuration changed
            // without OPUS_STATE_BYTES being re-measured.
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
        // libopus counts samples per channel; the trait's contract is the
        // interleaved total, so this is the one place the two disagree.
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
        // This is the real adapter, not the stub — it pins the per-channel to
        // interleaved conversion against genuine libopus output.
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
