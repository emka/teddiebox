//! Pins how much RAM the container reader occupies.
//!
//! Without this, a new field could double the size unnoticed. On an ESP32-S3
//! that matters: the reader is moved by value, and a few kilobytes is a large
//! part of an embassy task stack.
//!
//! Measured on a 64-bit host, where pointers and indices are widest, so a
//! bound that holds here also holds on the 32-bit device. The bounds leave
//! some headroom: they catch accidental growth, not small changes.

use teddiebox_taf::{SlicePages, TafReader};

/// `TafReader` is 4 560 bytes: a 4 096-byte page buffer, the chapter table,
/// and the lacing cursors. The bound allows a field or two but not another
/// page-sized buffer.
const READER_BYTES: usize = 5 * 1024;

#[test]
fn the_reader_stays_within_its_memory_budget() {
    let actual = size_of::<TafReader<SlicePages>>();
    assert!(
        actual <= READER_BYTES,
        "TafReader is {actual} bytes, budget is {READER_BYTES}"
    );
}
