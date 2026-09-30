//! Raw declarations for the part of libopus the firmware calls.
//!
//! Only the decoder functions actually used are declared. Generated bindings
//! would need libclang in the build and would cover much more.
//!
//! The allocating constructor (`opus_decoder_create`) is left out on purpose.
//! [`opus_decoder_get_size`] and [`opus_decoder_init`] let the caller place
//! the decoder state, so the firmware needs no heap.

#![no_std]

use core::ffi::c_int;

/// Opaque decoder state.
///
/// libopus does not publish the layout: callers ask
/// [`opus_decoder_get_size`] how many bytes it needs and supply storage that
/// is at least that large and aligned for `c_int`.
#[repr(C)]
pub struct OpusDecoder {
    _opaque: [u8; 0],
}

/// No error.
pub const OPUS_OK: c_int = 0;
/// One or more invalid or out-of-range arguments.
pub const OPUS_BAD_ARG: c_int = -1;
/// Not enough room in the output buffer for the packet's frame.
pub const OPUS_BUFFER_TOO_SMALL: c_int = -2;
/// The compressed data passed is corrupted.
pub const OPUS_INVALID_PACKET: c_int = -4;

extern "C" {
    /// Bytes of state a decoder for `channels` channels needs.
    pub fn opus_decoder_get_size(channels: c_int) -> c_int;

    /// Initialises decoder state in caller-supplied storage.
    ///
    /// # Safety
    ///
    /// `st` must point to at least [`opus_decoder_get_size`] writable bytes,
    /// aligned for `c_int`. `fs` must be one of 8000, 12000, 16000, 24000 or
    /// 48000, and `channels` 1 or 2.
    pub fn opus_decoder_init(st: *mut OpusDecoder, fs: i32, channels: c_int) -> c_int;

    /// Decodes one packet, returning samples **per channel** — not the
    /// interleaved total — or a negative error code.
    ///
    /// # Safety
    ///
    /// `st` must have been initialised by [`opus_decoder_init`]. `data` must
    /// be valid for `len` bytes, and `pcm` writable for
    /// `frame_size * channels` samples.
    pub fn opus_decode(
        st: *mut OpusDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linked_libopus_reports_a_stereo_decoder_state_size() {
        // Given: checks the library is found, linked and callable. The
        // declarations above cannot be checked by the compiler.
        let stereo = 2;

        // When
        // SAFETY: takes no pointers; 2 is a channel count libopus accepts.
        let size = unsafe { opus_decoder_get_size(stereo) };

        // Then
        assert!(
            size > 0,
            "libopus reported {size} bytes for a stereo decoder"
        );
    }
}
