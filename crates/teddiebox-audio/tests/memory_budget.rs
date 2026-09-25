//! Pins what the whole decode path costs in RAM.
//!
//! The board has no PSRAM, so all of this must fit in internal SRAM, and the
//! firmware is designed around the total. Measured on a 64-bit host, which
//! overstates the 32-bit device, so a bound that holds here holds there.

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, OPUS_STATE_BYTES};
use teddiebox_taf::{SlicePages, PAGE_SIZE};

/// `TafDecoder` is 8 680 bytes: the reader plus its own packet buffer. It
/// owns the reader, so this is the size that matters.
///
/// The packet buffer is as big as the reader's page buffer, and each packet
/// is copied from one to the other. A `next_packet` that borrowed from the
/// page instead of copying would save 4 KB, if RAM ever gets tight.
const DECODER_BYTES: usize = 9 * 1024;

/// What must be in memory at once to decode a frame: the decoder, the codec
/// state it borrows, and the temporary page `TafReader::load_page` checks
/// before using it. That last one is on the stack, not a field, so it is
/// easy to forget.
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
    let state = size_of::<OpusState>();
    let total = decoder + state + PAGE_SIZE;

    assert!(
        total <= DECODE_PATH_BYTES,
        "decoding needs {total} bytes live ({decoder} decoder + {state} codec state \
         + {PAGE_SIZE} page scratch), budget is {DECODE_PATH_BYTES}"
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
