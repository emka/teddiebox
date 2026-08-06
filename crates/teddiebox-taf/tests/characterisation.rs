//! Locks down the observed on-disk layout of a real .taf file.
//!
//! These tests do not test our code. They fail if the format assumptions the
//! parser is built on ever stop holding.
//!
//! The fixture was generated with `tools/fixturegen` from the `toniefile`
//! crate (audio_id `0x1234_5678`, a 5 s 440 Hz stereo sine tone). Two of the
//! four claims in the original plan did not survive contact with the real
//! file -- see the doc comments below on the two tests that replace them.

use teddiebox_taf::PAGE_SIZE;

const FIXTURE: &[u8] = include_bytes!("data/sine.taf");

#[test]
fn file_is_a_whole_number_of_pages() {
    assert_eq!(FIXTURE.len() % PAGE_SIZE, 0);
    assert!(FIXTURE.len() >= 2 * PAGE_SIZE);
}

#[test]
fn header_page_starts_with_big_endian_protobuf_length() {
    let len = u32::from_be_bytes(FIXTURE[0..4].try_into().unwrap()) as usize;
    assert!(len > 0);
    assert!(len <= PAGE_SIZE - 4);
}

/// The plan predicted a length-prefixed protobuf blob followed by `0xFF`
/// filler bytes out to the page boundary, appended *after* the message.
///
/// That is not what `toniefile` produces. The observed length prefix is
/// exactly `PAGE_SIZE - 4` (4092): the protobuf message itself is sized to
/// consume the whole of page 0, with no bytes left over for external
/// padding at all. Whatever padding exists lives *inside* the protobuf
/// message, as the content of its own trailing field -- see
/// `header_tail_is_zero_padded_inside_the_protobuf_message` below.
#[test]
fn header_protobuf_message_fills_the_entire_page() {
    let len = u32::from_be_bytes(FIXTURE[0..4].try_into().unwrap()) as usize;
    assert_eq!(
        4 + len,
        PAGE_SIZE,
        "expected the length-prefixed protobuf header to exactly fill page 0, \
         leaving no room for padding outside the message"
    );
}

/// The plan predicted the page-0 filler bytes are `0xFF`. The real fixture
/// has zero bytes (`0x00`) there instead, running from partway through the
/// protobuf message's last field (observed to start at file offset 42 for
/// this fixture) through to the end of the page. This checks a generous,
/// field-boundary-independent suffix of the page rather than that exact
/// offset, so it doesn't overfit to this one encoding's varint widths.
#[test]
fn header_tail_is_zero_padded_inside_the_protobuf_message() {
    let tail = &FIXTURE[PAGE_SIZE - 4000..PAGE_SIZE];
    assert!(
        tail.iter().all(|&b| b == 0x00),
        "expected zero padding in the tail of the header protobuf message, not 0xFF"
    );
}

#[test]
fn every_page_after_the_header_is_an_ogg_page() {
    for (i, page) in FIXTURE
        .as_chunks::<PAGE_SIZE>()
        .0
        .iter()
        .enumerate()
        .skip(1)
    {
        assert_eq!(&page[0..4], b"OggS", "page {i} is not an Ogg page");
    }
}

/// Every Ogg page in the file — including the small ones packed in behind
/// another inside a single block — carries one stream serial, and it equals
/// the header's `audio_id`.
///
/// The reader enforces only the first half: pages must agree with each
/// other. The equality is recorded here rather than checked in the parser
/// because it rests on `toniefile` alone, and a real Toniebox file has never
/// been examined. Enforcing it would make every commercial `.taf` fail to
/// open if the convention turns out to be `toniefile`'s rather than
/// Boxine's. If a real file confirms it, tightening the reader is one line.
#[test]
fn every_ogg_page_carries_the_audio_id_as_its_stream_serial() {
    let mut serials = Vec::new();
    let mut offset = PAGE_SIZE;
    while let Some(found) = find_capture_pattern(FIXTURE, offset) {
        serials.push(u32::from_le_bytes(
            FIXTURE[found + 14..found + 18].try_into().unwrap(),
        ));
        offset = found + 1;
    }

    assert!(
        serials.len() > 1,
        "expected several pages, found {serials:?}"
    );
    assert!(
        serials.iter().all(|&s| s == 0x1234_5678),
        "expected every page to carry audio_id 0x12345678 as its serial, got {serials:?}"
    );
}

fn find_capture_pattern(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(4)
        .position(|w| w == b"OggS")
        .map(|p| from + p)
}

#[test]
fn the_real_fixture_header_parses() {
    let page: &[u8; PAGE_SIZE] = FIXTURE[0..PAGE_SIZE].try_into().unwrap();
    let h = teddiebox_taf::TonieHeader::parse(page).expect("header should parse");
    assert_eq!(h.audio_id, 0x1234_5678, "audio id set by fixturegen");
    assert!(h.data_length > 0);
}
