//! Pins how much RAM the container reader occupies.
//!
//! These numbers were measured for the first time at M1's review and nothing
//! held them there, so a field added in passing could have doubled them
//! unnoticed. On an ESP32-S3 that matters: the reader is moved by value, and
//! a few kilobytes is a large fraction of an embassy task stack.
//!
//! Measured on a 64-bit host, where every pointer and index is as wide as it
//! ever gets, so a bound that holds here holds on the 32-bit device too.
//! The bounds carry headroom deliberately — this is a tripwire against
//! accidental growth, not a target to optimise against.

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
