//! Pins how much RAM the container reader occupies.
//!
//! On an ESP32-S3 the reader lives inside nested async functions, and each
//! level copies its value between stack frames, so every byte of the reader
//! is paid for several times over on the stack.
//!
//! Measured on a 64-bit host, where pointers and indices are widest, so a
//! bound that holds here also holds on the 32-bit device.

use teddiebox_taf::{SlicePages, TafReader, PAGE_SIZE};

/// The reader borrows its block buffer and holds only the chapter table and
/// the lacing cursors. A quarter of a block leaves room for a field or two,
/// but not for a block buffer held inline again.
const READER_BYTES: usize = PAGE_SIZE / 4;

#[test]
fn the_reader_stays_within_its_memory_budget() {
    // Given: the reader as the firmware holds it

    // When
    let actual = size_of::<TafReader<SlicePages>>();

    // Then
    assert!(
        actual <= READER_BYTES,
        "TafReader is {actual} bytes, budget is {READER_BYTES}"
    );
}
