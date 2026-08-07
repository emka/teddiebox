//! Pins what the whole decode path costs in RAM.
//!
//! Phase B has to place this in internal SRAM — the board carries no PSRAM —
//! so the total is a number the firmware is designed around rather than a
//! detail. Measured on a 64-bit host, which over-states the
//! 32-bit device — a bound that holds here holds there.

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, OPUS_STATE_BYTES};
use teddiebox_taf::{SlicePages, PAGE_SIZE};

/// `TafDecoder` is 8 680 bytes: the reader, plus its own packet buffer. It
/// owns the reader, so this is the figure that matters for placement, not
/// the reader's alone.
///
/// It grew by 2 821 bytes when `MAX_PACKET` was raised to the page size,
/// which a real Toniebox file forced. The packet buffer is now the same
/// size as the reader's page buffer, and the packet is copied between them
/// — so a `next_packet` that borrowed from the page instead of copying
/// would give all 4 KB back. Worth doing if M4 finds RAM tight; not worth
/// the API churn on speculation.
const DECODER_BYTES: usize = 9 * 1024;

/// What has to be live at once to decode a frame: the decoder, the codec
/// state it borrows, and the page `TafReader::load_page` validates into
/// before committing it. That last one is transient stack rather than a
/// field, which is exactly why it is easy to forget and worth naming here.
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
    // Not a size limit but a placement claim: the codec state alone is
    // several times a typical embassy task stack, which is why `LibOpus`
    // borrows it instead of owning it. If this ever stops being true the
    // borrowing API has lost its reason to exist and should be simplified.
    let decoder = size_of::<TafDecoder<SlicePages, LibOpus<'static>>>();

    assert!(
        OPUS_STATE_BYTES > decoder,
        "codec state is {OPUS_STATE_BYTES} bytes against a {decoder}-byte decoder"
    );
}
