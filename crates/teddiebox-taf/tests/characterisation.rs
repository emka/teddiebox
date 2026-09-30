//! Checks the on-disk layout of a real .taf file.
//!
//! These tests do not test our code. They fail if the format assumptions the
//! parser relies on stop holding.
//!
//! The fixture was written by the `toniefile` crate 0.1.1, a TAF writer
//! independent of this repository (audio_id `0x1234_5678`, 5 s of stereo
//! tone: 440 Hz left, 660 Hz right, so a swapped channel can be detected). It
//! is frozen test data that nothing here regenerates, so the parser is checked
//! against another implementation's reading of the format, not its own.

use teddiebox_taf::PAGE_SIZE;

const FIXTURE: &[u8] = include_bytes!("data/sine.taf");

#[test]
fn file_is_a_whole_number_of_pages() {
    // Given
    let file = FIXTURE;

    // When
    let (whole_pages, leftover) = (file.len() / PAGE_SIZE, file.len() % PAGE_SIZE);

    // Then
    assert_eq!(leftover, 0);
    assert!(whole_pages >= 2);
}

#[test]
fn header_page_starts_with_big_endian_protobuf_length() {
    // Given
    let file = FIXTURE;

    // When
    let len = u32::from_be_bytes(file[0..4].try_into().unwrap()) as usize;

    // Then
    assert!(len > 0);
    assert!(len <= PAGE_SIZE - 4);
}

/// The length prefix is exactly `PAGE_SIZE - 4` (4092): the protobuf message
/// fills the whole of page 0, with no padding after it. The padding is
/// inside the message, in its last field. See
/// `header_tail_is_zero_padded_inside_the_protobuf_message` below.
#[test]
fn header_protobuf_message_fills_the_entire_page() {
    // Given
    let file = FIXTURE;

    // When
    let len = u32::from_be_bytes(file[0..4].try_into().unwrap()) as usize;

    // Then
    assert_eq!(
        4 + len,
        PAGE_SIZE,
        "expected the length-prefixed protobuf header to exactly fill page 0, \
         leaving no room for padding outside the message"
    );
}

/// The padding in page 0 is zero bytes (`0x00`), not `0xFF`. It runs from
/// inside the protobuf message's last field (file offset 42 in this fixture)
/// to the end of the page. The test checks a generous suffix of the page, so
/// it does not depend on the exact offset.
#[test]
fn header_tail_is_zero_padded_inside_the_protobuf_message() {
    // Given
    let file = FIXTURE;

    // When
    let tail = &file[PAGE_SIZE - 4000..PAGE_SIZE];

    // Then
    assert!(
        tail.iter().all(|&b| b == 0x00),
        "expected zero padding in the tail of the header protobuf message, not 0xFF"
    );
}

#[test]
fn every_page_after_the_header_is_an_ogg_page() {
    // Given
    let file = FIXTURE;

    // When
    let not_ogg: Vec<usize> = file
        .as_chunks::<PAGE_SIZE>()
        .0
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, page)| &page[0..4] != b"OggS")
        .map(|(i, _)| i)
        .collect();

    // Then
    assert_eq!(not_ogg, Vec::<usize>::new(), "pages that are not Ogg pages");
}

/// Every Ogg page in the file — including the small ones packed in behind
/// another inside a single block — carries one stream serial, and it equals
/// the header's `audio_id`.
///
/// Three real commercial files (22,110 Ogg pages) follow the same rule, and
/// `TafReader::check_stream` enforces it.
#[test]
fn every_ogg_page_carries_the_audio_id_as_its_stream_serial() {
    // Given
    let file = FIXTURE;

    // When
    let mut serials = Vec::new();
    let mut offset = PAGE_SIZE;
    while let Some(found) = find_capture_pattern(file, offset) {
        serials.push(u32::from_le_bytes(
            file[found + 14..found + 18].try_into().unwrap(),
        ));
        offset = found + 1;
    }

    // Then
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
    // Given
    let page: &[u8; PAGE_SIZE] = FIXTURE[0..PAGE_SIZE].try_into().unwrap();

    // When
    let h = teddiebox_taf::TonieHeader::parse(page).expect("header should parse");

    // Then
    assert_eq!(
        h.audio_id, 0x1234_5678,
        "audio id the fixture was written with"
    );
    assert!(h.data_length > 0);
}
