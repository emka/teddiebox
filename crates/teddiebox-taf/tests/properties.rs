//! Properties of the parser over input nobody wrote to be valid: a card can
//! hold a damaged file, and a download can be cut short.
//!
//! Each property says the parser returns, with a value or an error, and never
//! panics or loops. On the box a panic is a reset, and a loop is a box that
//! stops responding.
//!
//! The seed is fixed, so a failure reproduces on every run. Finding new
//! inputs is the fuzzer's job, not the unit suite's.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use teddiebox_taf::{SlicePages, TafReader, TonieHeader, MAX_CHAPTERS, MAX_PACKET, PAGE_SIZE};

const FIXTURE: &[u8] = include_bytes!("data/sine.taf");

fn config() -> Config {
    Config {
        rng_seed: RngSeed::Fixed(0x7ed_d1e),
        failure_persistence: None,
        ..Config::default()
    }
}

/// Reads every packet, stopping at the end of the stream or the first error.
///
/// A page holds at most 255 lacing segments, so no file of `pages` pages can
/// yield more than `pages * 255` packets. Reaching that many means the
/// reader is going round in circles.
fn read_to_end(reader: &mut TafReader<SlicePages<'_>>, pages: usize) -> Result<(), String> {
    let mut packet = [0u8; MAX_PACKET];
    for _ in 0..=pages * 255 {
        match reader.next_packet(&mut packet) {
            Ok(Some(len)) if len > packet.len() => {
                return Err(format!(
                    "reported a {len}-byte packet in a {MAX_PACKET}-byte buffer"
                ))
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Ok(()),
        }
    }
    Err(format!("still reading after {} packets", pages * 255 + 1))
}

/// Opens `file` and reads it from the start and from `chapter`, as the box
/// does for a figure placed anew and for a skip.
fn exercise(file: &[u8], chapter: usize) -> Result<(), String> {
    let pages = file.len().div_ceil(PAGE_SIZE);
    let Ok(source) = SlicePages::new(file) else {
        return Ok(());
    };
    let Ok(mut reader) = TafReader::open(source) else {
        return Ok(());
    };
    read_to_end(&mut reader, pages)?;
    if reader.seek_to_chapter(chapter).is_ok() {
        read_to_end(&mut reader, pages)?;
    }
    Ok(())
}

/// Protobuf's base-128 varint, as the format specifies it.
fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A varint's value, with the ones that break arithmetic made likely: a
/// length at the edge of the page, and the 10-byte values so close to
/// `u64::MAX` that adding a position within the message overflows.
fn awkward_u64() -> impl Strategy<Value = u64> {
    prop_oneof![
        0..=64u64,
        (PAGE_SIZE as u64 - 16)..=(PAGE_SIZE as u64 + 16),
        any::<u64>(),
        (u64::MAX - 64)..=u64::MAX,
    ]
}

/// One protobuf field of any number and any wire type, valid or not. Length
/// fields claim `value` bytes whatever follows them.
fn field() -> impl Strategy<Value = Vec<u8>> {
    (
        0..8u64,
        0..8u64,
        awkward_u64(),
        prop::collection::vec(any::<u8>(), 0..32),
    )
        .prop_map(|(number, wire, value, payload)| {
            let mut out = Vec::new();
            varint(number << 3 | wire, &mut out);
            match wire {
                0 => varint(value, &mut out),
                1 => out.extend_from_slice(&value.to_le_bytes()),
                2 => {
                    varint(value, &mut out);
                    out.extend(payload);
                }
                5 => out.extend_from_slice(&(value as u32).to_le_bytes()),
                _ => {}
            }
            out
        })
}

/// A header page as the format lays it out: a big-endian length that fills
/// the page, then protobuf fields, then zeros. The fields may run past the
/// end of the message.
fn header_page() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(field(), 0..12).prop_map(|fields| {
        let mut page = ((PAGE_SIZE - 4) as u32).to_be_bytes().to_vec();
        page.extend(fields.concat());
        page.resize(PAGE_SIZE, 0);
        page
    })
}

/// The fixture's real header page followed by one to three pages of random
/// bytes, so the reader opens the file and then meets pages that are not Ogg.
fn real_header_then_noise() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), PAGE_SIZE..=3 * PAGE_SIZE).prop_map(|noise| {
        let mut file = FIXTURE[..PAGE_SIZE].to_vec();
        file.extend(noise);
        file
    })
}

/// Where to damage the fixture: anywhere, or in the first bytes of a block,
/// where the Ogg page header and its segment table are. Most of a block is
/// audio the parser never looks inside, so damage there tells it nothing.
fn damage_offset() -> impl Strategy<Value = usize> {
    let blocks = FIXTURE.len() / PAGE_SIZE;
    prop_oneof![
        0..FIXTURE.len(),
        (1..blocks, 0..27 + 255usize).prop_map(|(block, at)| block * PAGE_SIZE + at),
    ]
}

/// Up to eight bytes of the fixture overwritten.
fn damaged_fixture() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec((damage_offset(), any::<u8>()), 1..=8).prop_map(|damage| {
        let mut file = FIXTURE.to_vec();
        for (at, byte) in damage {
            file[at] = byte;
        }
        file
    })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn any_header_page_parses_or_is_refused(page in header_page()) {
        let page: &[u8; PAGE_SIZE] = page.as_slice().try_into().unwrap();
        let _ = TonieHeader::parse(page);
    }

    #[test]
    fn a_damaged_file_is_read_to_an_end(
        file in damaged_fixture(),
        chapter in 0..=MAX_CHAPTERS + 1,
    ) {
        exercise(&file, chapter).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn a_real_header_over_noise_is_read_to_an_end(
        file in real_header_then_noise(),
        chapter in 0..=MAX_CHAPTERS + 1,
    ) {
        exercise(&file, chapter).map_err(TestCaseError::fail)?;
    }
}
