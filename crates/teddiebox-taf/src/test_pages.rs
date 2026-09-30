//! Builders for hand-made TAF pages, shared by the tests of every module that
//! reads them.

use crate::PAGE_SIZE;

/// A header page carrying the given protobuf field bytes behind their length
/// prefix, the rest of the page filled with 0xFF.
pub(crate) fn header_page(fields: &[u8]) -> [u8; PAGE_SIZE] {
    let mut page = [0xFFu8; PAGE_SIZE];
    page[0..4].copy_from_slice(&(fields.len() as u32).to_be_bytes());
    page[4..4 + fields.len()].copy_from_slice(fields);
    page
}

/// A minimal valid Ogg page carrying `packets`, with stream serial 0.
pub(crate) fn ogg_page(packets: &[&[u8]]) -> [u8; PAGE_SIZE] {
    ogg_page_with_serial(0, packets)
}

/// Like [`ogg_page`], but with a stream serial.
pub(crate) fn ogg_page_with_serial(serial: u32, packets: &[&[u8]]) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    page[0..4].copy_from_slice(b"OggS");
    page[14..18].copy_from_slice(&serial.to_le_bytes());

    let mut off = 27usize;
    for p in packets {
        let mut remaining = p.len();
        while remaining >= 255 {
            page[off] = 255;
            off += 1;
            remaining -= 255;
        }
        page[off] = remaining as u8;
        off += 1;
    }
    let segment_count = off - 27;
    page[26] = segment_count as u8;

    for p in packets {
        page[off..off + p.len()].copy_from_slice(p);
        off += p.len();
    }
    page
}
