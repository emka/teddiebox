//! Makes the xtensa linker pull in the whole decode path.
//!
//! `cargo check` shows the Rust compiles for the device, but not that the
//! calls into libopus resolve, or that libopus's own needs (`memcpy`, libm)
//! are met by the ESP toolchain's C library. That only shows at link time,
//! which needs code that calls it. This is that code.
//!
//! Not firmware, never flashed. Only `scripts/xtensa-link-check.sh` builds
//! and links it.

#![no_std]

use core::panic::PanicInfo;
use core::slice;

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, MAX_FRAME_SAMPLES};
use teddiebox_taf::SlicePages;

/// Decodes the first audio frame of an in-memory TAF image.
///
/// Returns the interleaved sample count, 0 at end of stream, or a negative
/// code identifying which layer refused.
///
/// # Safety
///
/// `taf` must be valid for `taf_len` bytes, `pcm` writable for
/// [`MAX_FRAME_SAMPLES`] samples, and `state` a valid [`OpusState`].
#[no_mangle]
pub unsafe extern "C" fn teddiebox_decode_first_frame(
    taf: *const u8,
    taf_len: usize,
    pcm: *mut i16,
    state: *mut OpusState,
) -> i32 {
    let taf = slice::from_raw_parts(taf, taf_len);
    let pcm = slice::from_raw_parts_mut(pcm, MAX_FRAME_SAMPLES);

    let Ok(source) = SlicePages::new(taf) else {
        return -1;
    };
    let Ok(opus) = LibOpus::new(&mut *state) else {
        return -2;
    };
    let Ok(mut decoder) = TafDecoder::open(source, opus) else {
        return -3;
    };

    match decoder.next_frame(pcm) {
        Ok(Some(n)) => n as i32,
        Ok(None) => 0,
        Err(_) => -4,
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}
