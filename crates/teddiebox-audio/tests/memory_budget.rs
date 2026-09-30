//! Pins what the whole decode path costs in RAM.
//!
//! The board has no PSRAM, so all of this must fit in internal SRAM, and the
//! firmware is designed around the total. Measured on a 64-bit host, which
//! overstates the 32-bit device, so a bound that holds here holds there.

use teddiebox_audio::{LibOpus, OpusState, TafBuffers, TafDecoder, OPUS_STATE_BYTES};
use teddiebox_taf::{SlicePages, PAGE_SIZE};

/// The decoder borrows both of its blocks, so it holds little more than the
/// reader. It lives inside nested async functions on the box, and each level
/// copies it between stack frames, so the bound is a quarter of a block: a
/// block held inline again breaks it.
const DECODER_BYTES: usize = PAGE_SIZE / 4;

/// What must be in memory at once to decode a frame: the decoder, the blocks
/// and the codec state it borrows.
const DECODE_PATH_BYTES: usize = 43 * 1024;

#[test]
fn the_decoder_stays_within_its_memory_budget() {
    let actual = size_of::<TafDecoder<SlicePages, LibOpus<'static>>>();
    assert!(
        actual <= DECODER_BYTES,
        "TafDecoder is {actual} bytes, budget is {DECODER_BYTES}"
    );
}

#[test]
fn the_whole_decode_path_stays_within_its_memory_budget() {
    let decoder = size_of::<TafDecoder<SlicePages, LibOpus<'static>>>();
    let buffers = size_of::<TafBuffers>();
    let state = size_of::<OpusState>();
    let total = decoder + buffers + state;

    assert!(
        total <= DECODE_PATH_BYTES,
        "decoding needs {total} bytes live ({decoder} decoder + {buffers} blocks \
         + {state} codec state), budget is {DECODE_PATH_BYTES}"
    );
}

#[test]
fn the_codec_state_dominates_the_budget_and_so_cannot_live_on_a_task_stack() {
    // The codec state alone is several times a typical embassy task stack,
    // which is why `LibOpus` borrows it instead of owning it. If this stops
    // being true, the borrowing API is no longer needed.
    let decoder = size_of::<TafDecoder<SlicePages, LibOpus<'static>>>();

    assert!(
        OPUS_STATE_BYTES > decoder,
        "codec state is {OPUS_STATE_BYTES} bytes against a {decoder}-byte decoder"
    );
}
