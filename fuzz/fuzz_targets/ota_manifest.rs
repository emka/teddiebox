//! An update manifest, as whoever controls the update server sends it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_ota::{Manifest, MAX_MANIFEST};

fuzz_target!(|data: &[u8]| {
    let _ = Manifest::parse_read(data, MAX_MANIFEST);
});
