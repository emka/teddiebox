//! The TAF header page, as the box reads it from the card.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_taf::{TonieHeader, PAGE_SIZE};

fuzz_target!(|data: &[u8]| {
    let mut page = [0u8; PAGE_SIZE];
    let len = data.len().min(PAGE_SIZE);
    page[..len].copy_from_slice(&data[..len]);
    let _ = TonieHeader::parse(&page);
});
