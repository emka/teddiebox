//! A whole TAF file, read from the start and then from a chapter, as the box
//! does for a figure placed anew and for a skip.
//!
//! The last byte picks the chapter: in a real file it is audio, so a seed
//! from `crates/teddiebox-taf/tests/data` still reads as that file.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_taf::{SlicePages, TafReader, MAX_CHAPTERS, MAX_PACKET, PAGE_SIZE};

/// Reads every packet, stopping at the end of the stream or the first error.
///
/// A page holds at most 255 lacing segments, so no file of `pages` pages can
/// yield more than `pages * 255` packets. Reaching that many means the
/// reader is going round in circles.
fn read_to_end(reader: &mut TafReader<SlicePages<'_>>, pages: usize) {
    let mut packet = [0u8; MAX_PACKET];
    for _ in 0..=pages * 255 {
        match reader.next_packet(&mut packet) {
            Ok(Some(len)) => assert!(len <= packet.len(), "a {len}-byte packet"),
            Ok(None) | Err(_) => return,
        }
    }
    panic!("still reading after {} packets", pages * 255 + 1);
}

fuzz_target!(|data: &[u8]| {
    let Some(&last) = data.last() else { return };
    let chapter = usize::from(last) % (MAX_CHAPTERS + 2);
    let pages = data.len().div_ceil(PAGE_SIZE);
    let Ok(source) = SlicePages::new(data) else {
        return;
    };
    let mut page = [0u8; PAGE_SIZE];
    let Ok(mut reader) = TafReader::open(source, &mut page) else {
        return;
    };
    read_to_end(&mut reader, pages);
    if reader.seek_to_chapter(chapter).is_ok() {
        read_to_end(&mut reader, pages);
    }
});
