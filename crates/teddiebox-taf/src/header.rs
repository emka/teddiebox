//! The TAF header: a big-endian length prefix followed by a protobuf message
//! that fills the remainder of page 0.
//!
//! The message is padded to the page boundary from the inside, by a
//! zero-filled length-delimited field, rather than by trailing bytes after it.
//! The generic unknown-field skip handles that field like any other; no
//! special case is needed or wanted.

use crate::varint::read_varint;
use crate::{TafError, MAX_CHAPTERS, PAGE_SIZE};
use heapless::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TonieHeader {
    /// Identifies the content; also used as the cache key against teddyCloud.
    pub audio_id: u32,
    /// Length in bytes of the Ogg stream following the header page.
    pub data_length: u32,
    /// Ogg page indices at which each chapter starts.
    ///
    /// **Indices are relative to the Ogg stream, not the file.** Chapter page
    /// 0 is the first Ogg page, which lives at file page 1 — the header
    /// occupies file page 0. Confirmed by observation in Task 2: a
    /// single-chapter fixture carries `[0]`, not `[1]`. Callers that seek must
    /// add one.
    pub chapter_pages: Vec<u32, MAX_CHAPTERS>,
}

impl TonieHeader {
    pub fn parse(page: &[u8; PAGE_SIZE]) -> Result<Self, TafError> {
        let proto_len = u32::from_be_bytes([page[0], page[1], page[2], page[3]]) as usize;
        if proto_len == 0 || proto_len > PAGE_SIZE - 4 {
            return Err(TafError::MalformedHeader);
        }
        let body = &page[4..4 + proto_len];

        let mut header = TonieHeader {
            audio_id: 0,
            data_length: 0,
            chapter_pages: Vec::new(),
        };

        let mut pos = 0usize;
        while pos < body.len() {
            let key = read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)?;
            let field = key >> 3;
            let wire = key & 0x07;

            match (field, wire) {
                // data_length
                (2, 0) => {
                    header.data_length =
                        read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)? as u32;
                }
                // audio_id
                (3, 0) => {
                    header.audio_id =
                        read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)? as u32;
                }
                // chapter_pages, unpacked
                (4, 0) => {
                    let v = read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)? as u32;
                    header
                        .chapter_pages
                        .push(v)
                        .map_err(|_| TafError::TooManyChapters)?;
                }
                // chapter_pages, packed
                (4, 2) => {
                    let len =
                        read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)? as usize;
                    let end = pos.checked_add(len).ok_or(TafError::MalformedHeader)?;
                    if end > body.len() {
                        return Err(TafError::MalformedHeader);
                    }
                    while pos < end {
                        let v =
                            read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)? as u32;
                        header
                            .chapter_pages
                            .push(v)
                            .map_err(|_| TafError::TooManyChapters)?;
                    }
                    // A varint whose continuation bytes cross `end` is only
                    // caught here: `read_varint` itself isn't bounded by
                    // `end`, so it would otherwise read on into whatever
                    // follows the packed field instead of being rejected.
                    if pos != end {
                        return Err(TafError::MalformedHeader);
                    }
                }
                // Unknown field: skip by wire type.
                (_, 0) => {
                    read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)?;
                }
                (_, 2) => {
                    let len =
                        read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)? as usize;
                    pos = pos.checked_add(len).ok_or(TafError::MalformedHeader)?;
                    if pos > body.len() {
                        return Err(TafError::MalformedHeader);
                    }
                }
                // Fixed-width unknown fields must be bounds-checked like the
                // length-delimited case. Advancing past the end would leave the
                // `while pos < body.len()` loop simply false, returning Ok on a
                // truncated message instead of rejecting it.
                (_, 5) => {
                    pos = pos.checked_add(4).ok_or(TafError::MalformedHeader)?;
                    if pos > body.len() {
                        return Err(TafError::MalformedHeader);
                    }
                }
                (_, 1) => {
                    pos = pos.checked_add(8).ok_or(TafError::MalformedHeader)?;
                    if pos > body.len() {
                        return Err(TafError::MalformedHeader);
                    }
                }
                _ => return Err(TafError::MalformedHeader),
            }
        }

        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a header page the way a real encoder does, so the test exercises
    /// the same path as production input.
    fn header_page(fields: &[u8]) -> [u8; PAGE_SIZE] {
        let mut page = [0xFFu8; PAGE_SIZE];
        page[0..4].copy_from_slice(&(fields.len() as u32).to_be_bytes());
        page[4..4 + fields.len()].copy_from_slice(fields);
        page
    }

    #[test]
    fn parses_audio_id_and_data_length() {
        // field 2 (data_length) varint = 8192; field 3 (audio_id) varint = 0x1234
        let fields = [0x10, 0x80, 0x40, 0x18, 0xB4, 0x24];
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();
        assert_eq!(h.data_length, 8192);
        assert_eq!(h.audio_id, 0x1234);
    }

    #[test]
    fn parses_packed_chapter_pages() {
        // field 4, wire type 2, payload length 4, values 1, 50, 120, 1
        let fields = [0x22, 0x04, 0x01, 0x32, 0x78, 0x01];
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();
        assert_eq!(h.chapter_pages.as_slice(), &[1, 50, 120, 1]);
    }

    #[test]
    fn parses_unpacked_chapter_pages() {
        // field 4, wire type 0, repeated
        let fields = [0x20, 0x01, 0x20, 0x32];
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();
        assert_eq!(h.chapter_pages.as_slice(), &[1, 50]);
    }

    #[test]
    fn skips_unknown_fields() {
        // field 1 (hash), wire type 2, 3 bytes, then field 3 (audio_id)
        let fields = [0x0A, 0x03, 0xAA, 0xBB, 0xCC, 0x18, 0x07];
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();
        assert_eq!(h.audio_id, 7);
    }

    #[test]
    fn rejects_length_prefix_larger_than_the_page() {
        let mut page = [0xFFu8; PAGE_SIZE];
        page[0..4].copy_from_slice(&(PAGE_SIZE as u32).to_be_bytes());
        assert_eq!(TonieHeader::parse(&page), Err(TafError::MalformedHeader));
    }

    #[test]
    fn rejects_truncated_fixed32_field() {
        // field 9, wire type 5 (fixed32): declares a 4-byte payload but only
        // 2 bytes remain in the body.
        let fields = [0x4D, 0x00, 0x00];
        assert_eq!(
            TonieHeader::parse(&header_page(&fields)),
            Err(TafError::MalformedHeader)
        );
    }

    #[test]
    fn rejects_truncated_fixed64_field() {
        // field 9, wire type 1 (fixed64): declares an 8-byte payload but only
        // 2 bytes remain in the body.
        let fields = [0x49, 0x00, 0x00];
        assert_eq!(
            TonieHeader::parse(&header_page(&fields)),
            Err(TafError::MalformedHeader)
        );
    }

    #[test]
    fn rejects_packed_field_whose_final_varint_overruns_its_declared_length() {
        // field 4, wire type 2, declared length 2, payload [0x80, 0x80]: both
        // bytes carry a continuation bit, so the varint is unterminated
        // within the declared length and only resolves by reading the extra
        // trailing byte that follows the field.
        let fields = [0x22, 0x02, 0x80, 0x80, 0x00];
        assert_eq!(
            TonieHeader::parse(&header_page(&fields)),
            Err(TafError::MalformedHeader)
        );
    }
}
